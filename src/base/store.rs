use std::collections::HashMap;
use std::path::Path;

use heed::types::{Bytes, Str};
use heed::{CompactionOption, Database, Env, EnvOpenOptions};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::base::log_throttle::LogThrottle;
use crate::base::types::{Embedding, Entity, Kern};
use crate::quant::{QuantizationMode, QuantizedVec};

// Headroom is a DURABILITY requirement: a full env fails even the deletes that
// would free space (MDB_MAP_FULL).
const MAP_SIZE: usize = 16 * 1024 * 1024 * 1024;
const MAX_DBS: u32 = 4;
const COLD_EVICT_WARN_SECS: u64 = 300;
// How far the cold tier may run over its cap before a trim pass is worth its
// full-table decode. See `cold_cap_amortized`.
const COLD_CAP_SLACK: usize = 1024;

const KERN_DB: &str = "kern";
const COLD_DB: &str = "cold";
// Vectors live apart from their rows so a cosine scan of the tier reads 4 bytes
// per dimension instead of bincode-decoding a whole Entity per row.
const COLD_VEC_DB: &str = "cold_vec";
const META_DB: &str = "meta";
const META_KEY: &str = "graph";
// Own meta key (not in GraphMeta) so a store with no epoch row reads 0.
const EPOCH_KEY: &str = "epoch";
// Own meta key so an unstamped store is "unknown", never a mismatch.
const EMBED_KEY: &str = "embed";

// Version byte prepended ahead of the zstd frame so a reader rejects any other
// format instead of mis-decoding it. Alpha: exactly one version is ever
// decodable — a mismatch is a clean BadVersion, never a migration.
const FORMAT_VERSION: u8 = 10; // v10: removed legacy_network_id from ParamsV0
const ZSTD_LEVEL: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushOutcome {
	Flushed { epoch: u64 },
	RefusedStale { disk_epoch: u64, expected: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
	#[error("io: {0}")]
	Io(#[from] std::io::Error),
	#[error("lmdb: {0}")]
	Lmdb(#[from] heed::Error),
	#[error("bincode encode: {0}")]
	BincodeEncode(#[from] bincode::error::EncodeError),
	#[error("bincode decode: {0}")]
	BincodeDecode(#[from] bincode::error::DecodeError),
	#[error("bad value format version: {0}")]
	BadVersion(u8),
	// A load that would silently produce an empty graph over a non-empty store.
	// Surfaced as an error so callers fall into the epoch-0 fallback, where the
	// guarded flush REFUSES to write and absorbs disk instead — self-healing.
	// Swallowing it here is what turned a bad read into a wiped store.
	#[error("store has {kerns} kern rows but no root — refusing to load as empty")]
	RootMissing { kerns: usize },
}

// Shared by both backends so encodings never drift; the 1 GiB alloc cap rejects
// corrupt length prefixes (tests/persist_fuzz.rs).
pub(crate) fn bincode_cfg() -> impl bincode::config::Config {
	bincode::config::standard().with_limit::<{ 1024 * 1024 * 1024 }>()
}

// [ver] ++ zstd(bincode(v)).
fn encode_at<T: Serialize>(ver: u8, v: &T) -> Result<Vec<u8>, StoreError> {
	let raw = bincode::serde::encode_to_vec(v, bincode_cfg())?;
	let comp = zstd::encode_all(raw.as_slice(), ZSTD_LEVEL)?;
	let mut out = Vec::with_capacity(comp.len() + 1);
	out.push(ver);
	out.extend_from_slice(&comp);
	Ok(out)
}

fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>, StoreError> {
	encode_at(FORMAT_VERSION, v)
}

fn strip_version(bytes: &[u8]) -> Result<(u8, Vec<u8>), StoreError> {
	let (&ver, body) = bytes.split_first().ok_or(StoreError::BadVersion(0))?;
	if ver != FORMAT_VERSION {
		return Err(StoreError::BadVersion(ver));
	}
	Ok((ver, zstd::decode_all(body)?))
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
	let (_ver, raw) = strip_version(bytes)?;
	let (v, _) = bincode::serde::decode_from_slice(&raw, bincode_cfg())?;
	Ok(v)
}

// Do NOT persist QuantizedVec directly — its skip_serializing_if desyncs
// positional bincode. Every field here is always present.
#[derive(Serialize, Deserialize)]
pub struct StoredVec {
	pub scale: f32,
	pub q: Vec<i8>,
}

impl StoredVec {
	fn encode(v: &[f32]) -> Self {
		let qv = QuantizedVec::encode(v, QuantizationMode::Int8);
		StoredVec {
			scale: qv.scale,
			q: qv.q,
		}
	}

	fn decode(&self) -> Vec<f32> {
		self.q.iter().map(|&x| (x as f32) * self.scale).collect()
	}
}

// Entity's temporal fields are serde(skip) for byte-stable layout, so the store
// carries them here.
#[derive(Serialize, Deserialize, Default)]
pub struct StoredTemporal {
	pub valid_from: Option<std::time::SystemTime>,
	pub valid_to: Option<std::time::SystemTime>,
	pub invalidated_at: Option<std::time::SystemTime>,
}

impl StoredTemporal {
	fn is_set(e: &Entity) -> bool {
		e.valid_from.is_some() || e.valid_to.is_some() || e.invalidated_at.is_some()
	}

	fn of(e: &Entity) -> Self {
		StoredTemporal {
			valid_from: e.valid_from,
			valid_to: e.valid_to,
			invalidated_at: e.invalidated_at,
		}
	}

	fn apply(&self, e: &mut Entity) {
		e.valid_from = self.valid_from;
		e.valid_to = self.valid_to;
		e.invalidated_at = self.invalidated_at;
	}
}

// A cold row carries the temporal triple the same way StoredKern does — without
// it a cold-recovered revision is valid at every instant.
#[derive(Serialize, Deserialize)]
pub struct ColdRow {
	pub entity: Entity,
	pub temporal: StoredTemporal,
}

impl ColdRow {
	// The vector is stripped out to the COLD_VEC_DB side table, so scoring the
	// tier never decodes the row it belongs to. `cold_get`/`cold_all` put it back.
	fn of(e: &Entity) -> Self {
		let mut entity = e.clone();
		entity.vector = Embedding::default();
		ColdRow {
			entity,
			temporal: StoredTemporal::of(e),
		}
	}

	fn into_entity(self, vector: Embedding) -> Entity {
		let mut e = self.entity;
		e.vector = vector;
		self.temporal.apply(&mut e);
		e
	}
}

fn encode_vec(v: &[f32]) -> Vec<u8> {
	let mut out = Vec::with_capacity(v.len() * 4);
	for x in v {
		out.extend_from_slice(&x.to_le_bytes());
	}
	out
}

fn decode_vec_into(bytes: &[u8], out: &mut Vec<f32>) {
	out.clear();
	out.reserve(bytes.len() / 4);
	for c in bytes.chunks_exact(4) {
		out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
	}
}

fn decode_vec(bytes: &[u8]) -> Embedding {
	let mut out = Vec::new();
	decode_vec_into(bytes, &mut out);
	out.into()
}

fn encode_cold(row: &ColdRow) -> Result<Vec<u8>, StoreError> {
	encode_at(FORMAT_VERSION, row)
}

// A decode failure here is real corruption and must reach scan_with's warning
// instead of being swallowed by a fallback that succeeds.
fn decode_cold(bytes: &[u8]) -> Result<ColdRow, StoreError> {
	let (_ver, raw) = strip_version(bytes)?;
	let (row, _) = bincode::serde::decode_from_slice::<ColdRow, _>(&raw, bincode_cfg())?;
	Ok(row)
}

#[derive(Serialize, Deserialize)]
pub struct StoredKern {
	pub kern: Kern,
	pub entity_vecs: HashMap<String, StoredVec>,
	pub reason_vecs: HashMap<String, StoredVec>,
	pub temporal: HashMap<String, StoredTemporal>,
}

impl StoredKern {
	pub fn from_kern(k: &Kern) -> Self {
		let mut kern = k.clone();
		let mut entity_vecs = HashMap::new();
		let mut reason_vecs = HashMap::new();
		let mut temporal = HashMap::new();
		for (id, e) in kern.entities.iter_mut() {
			if !e.vector.is_empty() {
				entity_vecs.insert(id.clone(), StoredVec::encode(&e.vector));
			}
			if StoredTemporal::is_set(e) {
				temporal.insert(id.clone(), StoredTemporal::of(e));
			}
			// vector is restored from the side-map on load; gnn_vector is recomputed,
			// never persisted.
			e.vector = Embedding::default();
			e.gnn_vector = Embedding::default();
		}
		for (id, r) in kern.reasons.iter_mut() {
			if !r.vector.is_empty() {
				reason_vecs.insert(id.clone(), StoredVec::encode(&r.vector));
			}
			r.vector = Embedding::default();
		}
		StoredKern {
			kern,
			entity_vecs,
			reason_vecs,
			temporal,
		}
	}

	pub fn into_kern(self) -> Kern {
		let mut kern = self.kern;
		for (id, e) in kern.entities.iter_mut() {
			if let Some(q) = self.entity_vecs.get(id) {
				e.vector = q.decode().into();
			}
			if let Some(t) = self.temporal.get(id) {
				t.apply(e);
			}
		}
		for (id, r) in kern.reasons.iter_mut() {
			if let Some(q) = self.reason_vecs.get(id) {
				r.vector = q.decode().into();
			}
		}
		kern
	}
}

// Identity of the model that produced the stored vectors. A query embedded by a
// different model scores as noise against them — cosine truncates to the shorter
// side rather than failing, so nothing else would ever notice.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedStamp {
	pub model: String,
	pub dim: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedCheck {
	Adopted,
	Match,
	Mismatch {
		stored: EmbedStamp,
		current: EmbedStamp,
	},
}

#[derive(Serialize, Deserialize)]
struct GraphMeta {
	network_id: String,
	quant_mode: QuantizationMode,
}

// One embedded LMDB env per data_dir; LMDB gives many-reader / single-writer
// concurrency across processes.
pub struct Store {
	env: Env,
	kern: Database<Str, Bytes>,
	cold: Database<Str, Bytes>,
	cold_vec: Database<Str, Bytes>,
	meta: Database<Str, Bytes>,
	dir: std::path::PathBuf,
	cold_evicted: std::sync::atomic::AtomicU64,
	cold_evict_warn: LogThrottle,
	embed_mismatch_warn: LogThrottle,
	embed_mismatch: std::sync::atomic::AtomicBool,
}

impl Store {
	// All named databases are created up front so later read txns never miss one.
	pub fn open(dir: &str) -> Result<Self, StoreError> {
		std::fs::create_dir_all(dir)?;
		let path = Path::new(dir);
		// SAFETY: mmap-ing a file is unsafe iff another process truncates/corrupts
		// it underneath us. The data dir is kern-owned; the only writers are kern
		// processes, which coordinate through LMDB's own lock. No external truncation.
		let env = unsafe {
			EnvOpenOptions::new()
				.map_size(MAP_SIZE)
				.max_dbs(MAX_DBS)
				.open(path)?
		};
		// Killed processes (timeouts, a killed hub, crashed CLIs) leak reader
		// slots in lock.mdb; enough of them and every open fails MDB_READERS_FULL
		// and the daemon boots an empty graph. Reap them on every open.
		match env.clear_stale_readers() {
			Ok(0) => {}
			Ok(n) => {
				tracing::info!(target: "kern.store", cleared = n, "reaped stale LMDB reader slots")
			}
			Err(e) => {
				tracing::warn!(target: "kern.store", error = %e, "stale-reader reap failed; continuing")
			}
		}
		let mut wtxn = env.write_txn()?;
		let kern = env.create_database::<Str, Bytes>(&mut wtxn, Some(KERN_DB))?;
		let cold = env.create_database::<Str, Bytes>(&mut wtxn, Some(COLD_DB))?;
		let cold_vec = env.create_database::<Str, Bytes>(&mut wtxn, Some(COLD_VEC_DB))?;
		let meta = env.create_database::<Str, Bytes>(&mut wtxn, Some(META_DB))?;
		wtxn.commit()?;
		Ok(Self {
			env,
			kern,
			cold,
			cold_vec,
			meta,
			dir: path.to_path_buf(),
			cold_evicted: std::sync::atomic::AtomicU64::new(0),
			cold_evict_warn: LogThrottle::new(COLD_EVICT_WARN_SECS),
			embed_mismatch_warn: LogThrottle::new(COLD_EVICT_WARN_SECS),
			embed_mismatch: std::sync::atomic::AtomicBool::new(false),
		})
	}

	pub fn data_file_len(&self) -> u64 {
		std::fs::metadata(self.dir.join("data.mdb"))
			.map(|m| m.len())
			.unwrap_or(0)
	}

	fn put<T: Serialize>(
		&self,
		db: Database<Str, Bytes>,
		key: &str,
		value: &T,
	) -> Result<(), StoreError> {
		let bytes = encode(value)?;
		let mut wtxn = self.env.write_txn()?;
		db.put(&mut wtxn, key, &bytes)?;
		wtxn.commit()?;
		Ok(())
	}

	fn get<T: DeserializeOwned>(
		&self,
		db: Database<Str, Bytes>,
		key: &str,
	) -> Result<Option<T>, StoreError> {
		self.get_with(db, key, decode)
	}

	fn get_with<T>(
		&self,
		db: Database<Str, Bytes>,
		key: &str,
		decode_fn: impl Fn(&[u8]) -> Result<T, StoreError>,
	) -> Result<Option<T>, StoreError> {
		let rtxn = self.env.read_txn()?;
		match db.get(&rtxn, key)? {
			Some(b) => Ok(Some(decode_fn(b)?)),
			None => Ok(None),
		}
	}

	fn remove(&self, db: Database<Str, Bytes>, key: &str) -> Result<(), StoreError> {
		let mut wtxn = self.env.write_txn()?;
		db.delete(&mut wtxn, key)?;
		wtxn.commit()?;
		Ok(())
	}

	fn scan_with<T>(
		&self,
		db: Database<Str, Bytes>,
		decode_fn: impl Fn(&[u8]) -> Result<T, StoreError>,
	) -> Result<Vec<(String, T)>, StoreError> {
		let rtxn = self.env.read_txn()?;
		let mut out = Vec::new();
		for item in db.iter(&rtxn)? {
			let (k, v) = item?;
			match decode_fn(v) {
				Ok(val) => out.push((k.to_string(), val)),
				Err(e) => {
					tracing::warn!(target: "kern.store", key = %k, error = %e, "skipping corrupt value");
				}
			}
		}
		Ok(out)
	}

	// Missing/garbled reads as 0 — the epoch is advisory and must never fail a load.
	pub fn read_epoch(&self) -> u64 {
		self
			.get::<u64>(self.meta, EPOCH_KEY)
			.ok()
			.flatten()
			.unwrap_or(0)
	}

	// Missing/garbled reads as "unstamped" — health is diagnostic and must never
	// fail a load. check_embed_stamp must NOT use this: it cannot tell absent from
	// unreadable, and it writes.
	pub fn embed_stamp(&self) -> Option<EmbedStamp> {
		self.get::<EmbedStamp>(self.meta, EMBED_KEY).ok().flatten()
	}

	pub fn set_embed_stamp(&self, stamp: &EmbedStamp) -> Result<(), StoreError> {
		self
			.embed_mismatch
			.store(false, std::sync::atomic::Ordering::Relaxed);
		self.put(self.meta, EMBED_KEY, stamp)
	}

	// Loud and recorded, never fatal: intake and recall are fail-open, and a store
	// that refuses to open is worse than one that answers badly while saying so.
	// The mismatched stamp is left on disk — it still describes what is stored.
	pub fn check_embed_stamp(&self, current: &EmbedStamp) -> Result<EmbedCheck, StoreError> {
		// An unreadable stamp is NOT an unstamped store. Adopting over it would
		// destroy the only record of which model produced the stored vectors.
		let stored = self.get::<EmbedStamp>(self.meta, EMBED_KEY).map_err(|e| {
			tracing::error!(
				target: "kern.store",
				error = %e,
				"embedding stamp unreadable — leaving it intact; the stored model is unknown this run"
			);
			e
		})?;
		let Some(stored) = stored else {
			self.set_embed_stamp(current)?;
			tracing::info!(
				target: "kern.store",
				model = %current.model,
				dim = current.dim,
				"unstamped store adopts the configured embedding model"
			);
			return Ok(EmbedCheck::Adopted);
		};
		if stored == *current {
			// The operator may have reverted; a stale flag would keep accusing.
			self
				.embed_mismatch
				.store(false, std::sync::atomic::Ordering::Relaxed);
			return Ok(EmbedCheck::Match);
		}
		self
			.embed_mismatch
			.store(true, std::sync::atomic::Ordering::Relaxed);
		// Throttled: the check now runs on every flush, and save_graph_guarded
		// retries up to 5×, so an un-reembedded store would emit this per save
		// forever. The `embed_mismatch` flag is the durable signal; only the line
		// is rate-limited.
		if self.embed_mismatch_warn.allow() {
			tracing::error!(
				target: "kern.store",
				stored_model = %stored.model,
				stored_dim = stored.dim,
				current_model = %current.model,
				current_dim = current.dim,
				"embedding model changed — stored vectors no longer match query vectors; \
				 recall stays near zero until `kern reembed`"
			);
		}
		Ok(EmbedCheck::Mismatch {
			stored,
			current: current.clone(),
		})
	}

	pub fn embed_mismatch(&self) -> bool {
		self
			.embed_mismatch
			.load(std::sync::atomic::Ordering::Relaxed)
	}

	// Read inside the open txn so the guard's check and the write commit atomically.
	fn epoch_in(&self, wtxn: &heed::RwTxn) -> Result<u64, StoreError> {
		match self.meta.get(wtxn, EPOCH_KEY)? {
			Some(b) => Ok(decode::<u64>(b).unwrap_or(0)),
			None => Ok(0),
		}
	}

	// Destructive prune-and-write shared by both save paths; stamps next_epoch.
	// `spare` names rows that must survive the prune even though they are not in
	// `kerns`: an unloaded kern left RAM as pure residency, and its disk row is
	// the only copy — pruning it would turn an idle sweep into a wipe.
	fn write_snapshot(
		&self,
		wtxn: &mut heed::RwTxn,
		kerns: &HashMap<String, Kern>,
		network_id: &str,
		quant_mode: QuantizationMode,
		next_epoch: u64,
		spare: &std::collections::HashSet<String>,
	) -> Result<(), StoreError> {
		// Collect existing keys first (immutable borrow of the txn), then mutate —
		// can't hold the iterator borrow across put/delete.
		let existing: Vec<String> = {
			let mut v = Vec::new();
			for item in self.kern.iter(wtxn)? {
				let (k, _) = item?;
				v.push(k.to_string());
			}
			v
		};
		for id in existing {
			if !kerns.contains_key(&id) && !spare.contains(&id) {
				self.kern.delete(wtxn, id.as_str())?;
			}
		}
		for (id, kern) in kerns {
			let bytes = encode(&StoredKern::from_kern(kern))?;
			self.kern.put(wtxn, id.as_str(), &bytes)?;
		}
		let meta = GraphMeta {
			network_id: network_id.to_string(),
			quant_mode,
		};
		let meta_bytes = encode(&meta)?;
		self.meta.put(wtxn, META_KEY, &meta_bytes)?;
		let epoch_bytes = encode(&next_epoch)?;
		self.meta.put(wtxn, EPOCH_KEY, &epoch_bytes)?;
		Ok(())
	}

	// One atomic commit — a crash leaves old or new, never a torn mix.
	pub fn save_all_kerns(
		&self,
		kerns: &HashMap<String, Kern>,
		network_id: &str,
		quant_mode: QuantizationMode,
		spare: &std::collections::HashSet<String>,
	) -> Result<u64, StoreError> {
		let mut wtxn = self.env.write_txn()?;
		let next = self.epoch_in(&wtxn)?.wrapping_add(1);
		self.write_snapshot(&mut wtxn, kerns, network_id, quant_mode, next, spare)?;
		wtxn.commit()?;
		Ok(next)
	}

	// Writes ONLY while the on-disk epoch still equals `expected`, else refuses
	// untouched; check and write share one txn.
	pub fn flush_guarded(
		&self,
		kerns: &HashMap<String, Kern>,
		network_id: &str,
		quant_mode: QuantizationMode,
		expected: u64,
		spare: &std::collections::HashSet<String>,
	) -> Result<FlushOutcome, StoreError> {
		let mut wtxn = self.env.write_txn()?;
		let disk = self.epoch_in(&wtxn)?;
		if disk > expected {
			wtxn.abort();
			return Ok(FlushOutcome::RefusedStale {
				disk_epoch: disk,
				expected,
			});
		}
		let next = disk.wrapping_add(1);
		self.write_snapshot(&mut wtxn, kerns, network_id, quant_mode, next, spare)?;
		wtxn.commit()?;
		Ok(FlushOutcome::Flushed { epoch: next })
	}

	pub fn load_all_kerns(
		&self,
	) -> Result<(HashMap<String, Kern>, String, QuantizationMode), StoreError> {
		let stored: Vec<(String, StoredKern)> = self.scan_with(self.kern, decode::<StoredKern>)?;
		let mut kerns = HashMap::with_capacity(stored.len());
		for (id, sk) in stored {
			kerns.insert(id, sk.into_kern());
		}
		let (network_id, quant_mode) = match self.get::<GraphMeta>(self.meta, META_KEY)? {
			Some(m) => (m.network_id, m.quant_mode),
			None => (String::new(), QuantizationMode::None),
		};
		Ok((kerns, network_id, quant_mode))
	}

	pub fn save_one_kern(&self, kern: &Kern) -> Result<(), StoreError> {
		self.put(self.kern, &kern.id, &StoredKern::from_kern(kern))
	}

	pub fn load_one_kern(&self, id: &str) -> Result<Option<Kern>, StoreError> {
		Ok(
			self
				.get_with(self.kern, id, decode::<StoredKern>)?
				.map(StoredKern::into_kern),
		)
	}

	pub fn delete_one_kern(&self, id: &str) -> Result<(), StoreError> {
		self.remove(self.kern, id)
	}

	pub fn cold_spill(&self, entity: &Entity) -> Result<(), StoreError> {
		let bytes = encode_cold(&ColdRow::of(entity))?;
		let mut wtxn = self.env.write_txn()?;
		self.cold.put(&mut wtxn, &entity.id, &bytes)?;
		self
			.cold_vec
			.put(&mut wtxn, &entity.id, &encode_vec(&entity.vector))?;
		wtxn.commit()?;
		self.cold_cap_amortized(crate::base::constants::COLD_MAX_ENTRIES)?;
		Ok(())
	}

	pub fn cold_get(&self, id: &str) -> Result<Option<Entity>, StoreError> {
		let Some(row) = self.get_with(self.cold, id, decode_cold)? else {
			return Ok(None);
		};
		let rtxn = self.env.read_txn()?;
		let vector = self
			.cold_vec
			.get(&rtxn, id)?
			.map(decode_vec)
			.unwrap_or_default();
		Ok(Some(row.into_entity(vector)))
	}

	pub fn cold_all(&self) -> Result<Vec<Entity>, StoreError> {
		let vecs: HashMap<String, Embedding> = self
			.scan_with(self.cold_vec, |b| Ok(decode_vec(b)))?
			.into_iter()
			.collect();
		Ok(
			self
				.scan_with(self.cold, decode_cold)?
				.into_iter()
				.map(|(id, r)| {
					let v = vecs.get(&id).cloned().unwrap_or_default();
					r.into_entity(v)
				})
				.collect(),
		)
	}

	pub fn cold_put_all(&self, entities: &[Entity]) -> Result<(), StoreError> {
		let mut wtxn = self.env.write_txn()?;
		for e in entities {
			let bytes = encode_cold(&ColdRow::of(e))?;
			self.cold.put(&mut wtxn, &e.id, &bytes)?;
			self
				.cold_vec
				.put(&mut wtxn, &e.id, &encode_vec(&e.vector))?;
		}
		wtxn.commit()?;
		self.cold_cap_amortized(crate::base::constants::COLD_MAX_ENTRIES)?;
		Ok(())
	}

	// Scores off COLD_VEC_DB alone, then decodes only the k winners. The scan is
	// still every row — an ANN over the cold tier would be a second resident index
	// for the tier that exists to NOT be resident — but the per-row cost drops from
	// a full Entity decode to a memcpy, which is where ~90% of it was.
	pub fn cold_search(&self, query_vec: &[f32], k: usize) -> Result<Vec<(Entity, f64)>, StoreError> {
		if query_vec.is_empty() || k == 0 {
			return Ok(Vec::new());
		}
		let mut scored: Vec<(String, f64)> = Vec::new();
		{
			let rtxn = self.env.read_txn()?;
			let mut buf: Vec<f32> = Vec::with_capacity(query_vec.len());
			for item in self.cold_vec.iter(&rtxn)? {
				let (id, bytes) = item?;
				decode_vec_into(bytes, &mut buf);
				if buf.len() != query_vec.len() {
					continue;
				}
				let s = crate::base::math::cosine(query_vec, &buf);
				if s.is_finite() {
					scored.push((id.to_string(), s));
				}
			}
		}
		// Ties broken by id ascending so truncation is deterministic — LMDB scan
		// order must not decide which equal-cosine entities survive.
		scored.sort_by(|a, b| crate::base::util::cmp_rank(a.1, &a.0, b.1, &b.0));
		scored.truncate(k);
		let mut out = Vec::with_capacity(scored.len());
		for (id, s) in scored {
			if let Some(e) = self.cold_get(&id)? {
				out.push((e, s));
			}
		}
		Ok(out)
	}

	// `cold_cap` decodes EVERY row to sort by age. Calling it per spill means that
	// once the tier is full — the steady state it is designed to sit in — each
	// single eviction pays a full-table decode, so a GC sweep evicting V victims
	// costs V passes over 50k rows. Trigger only once the tier is a slack margin
	// over the cap, then cut all the way back to it: one pass per SLACK spills
	// instead of one per spill.
	//
	// Tradeoff: the tier may hold up to `max + SLACK` rows between passes. The cap
	// is a disk bound, not a correctness boundary, and 2% overshoot buys a ~500x
	// reduction in decode work. Direct callers of `cold_cap` still get the exact
	// cap, so nothing that asks for a hard trim gets a soft one.
	pub(crate) fn cold_cap_amortized(&self, max: usize) -> Result<(), StoreError> {
		let len = {
			let rtxn = self.env.read_txn()?;
			self.cold.len(&rtxn)? as usize
		};
		if len <= max.saturating_add(COLD_CAP_SLACK) {
			return Ok(());
		}
		self.cold_cap(max)
	}

	pub(crate) fn cold_cap(&self, max: usize) -> Result<(), StoreError> {
		let len = {
			let rtxn = self.env.read_txn()?;
			self.cold.len(&rtxn)? as usize
		};
		if len <= max {
			return Ok(());
		}
		let mut rows: Vec<(String, ColdRow)> = self.scan_with(self.cold, decode_cold)?;
		rows.sort_by_key(|r| std::cmp::Reverse(r.1.entity.created_at));
		let drop_ids: Vec<String> = rows.into_iter().skip(max).map(|(id, _)| id).collect();
		let mut wtxn = self.env.write_txn()?;
		for id in &drop_ids {
			self.cold.delete(&mut wtxn, id.as_str())?;
			self.cold_vec.delete(&mut wtxn, id.as_str())?;
		}
		wtxn.commit()?;
		if !drop_ids.is_empty() {
			let evicted = drop_ids.len();
			// The counter is the durable signal and is never throttled; the LINE is.
			// A full tier evicts on every spill, and one GC sweep spills once per
			// victim, so an unthrottled warn here drowns the log forever.
			let total = self
				.cold_evicted
				.fetch_add(evicted as u64, std::sync::atomic::Ordering::Relaxed)
				+ evicted as u64;
			if self.cold_evict_warn.allow() {
				tracing::warn!(
					target: "kern.store",
					evicted,
					cap = max,
					total_evicted = total,
					"cold tier over capacity — oldest entities permanently dropped (further evictions counted, not logged)"
				);
			}
		}
		Ok(())
	}

	// Process-lifetime total; a non-durable entity dropped here is gone for good,
	// so the count is the only trace it ever existed.
	pub fn cold_evicted(&self) -> u64 {
		self.cold_evicted.load(std::sync::atomic::Ordering::Relaxed)
	}
}

fn compact_tmp(dir: &Path) -> std::path::PathBuf {
	dir.join("data.mdb.compact")
}

// Caller MUST drop every env handle first; retries ride out Windows' async unmap lag.
fn swap_compacted(dir: &str) -> Result<(u64, u64), StoreError> {
	let path = Path::new(dir);
	let data = path.join("data.mdb");
	let tmp = compact_tmp(path);
	let old_len = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);

	let mut last_err = None;
	for attempt in 0..25 {
		match std::fs::rename(&tmp, &data) {
			Ok(()) => {
				let _ = std::fs::remove_file(path.join("lock.mdb"));
				let new_len = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);
				return Ok((old_len, new_len));
			}
			Err(e) => {
				last_err = Some(e);
				std::thread::sleep(std::time::Duration::from_millis(100 + attempt * 4));
			}
		}
	}
	let _ = std::fs::remove_file(&tmp);
	Err(StoreError::Io(last_err.unwrap_or_else(|| {
		std::io::Error::other("compaction swap failed")
	})))
}

// The only way to shrink LMDB's high-water mark. REQUIRES exclusive access:
// run offline, daemon stopped.
pub fn compact_dir(dir: &str) -> Result<(u64, u64), StoreError> {
	let path = Path::new(dir);
	// A full env copy costs more than the space it could reclaim below this size.
	let old_len = std::fs::metadata(path.join("data.mdb"))
		.map(|m| m.len())
		.unwrap_or(0);
	if old_len < crate::base::constants::COLD_COMPACT_MIN_BYTES {
		return Ok((old_len, old_len));
	}

	let tmp = compact_tmp(path);
	let _ = std::fs::remove_file(&tmp);

	{
		let env = unsafe {
			EnvOpenOptions::new()
				.map_size(MAP_SIZE)
				.max_dbs(MAX_DBS)
				.open(path)?
		};
		env.copy_to_file(&tmp, CompactionOption::Enabled)?;
		// Block until the env is truly closed (mmap released) before the swap.
		env.prepare_for_closing().wait();
	}

	swap_compacted(dir)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base::types::{mk_entity, EntityKind, ReviewState};
	use std::time::{Duration, UNIX_EPOCH};

	#[derive(Debug, PartialEq, Serialize, Deserialize)]
	struct Sample {
		name: String,
		nums: Vec<f64>,
	}

	fn tmp() -> tempfile::TempDir {
		tempfile::tempdir().unwrap()
	}

	fn dir_of(d: &tempfile::TempDir) -> String {
		d.path().to_string_lossy().to_string()
	}

	#[test]
	fn codec_roundtrips_a_struct() {
		let v = Sample {
			name: "hello".into(),
			nums: vec![1.0, -2.5, 3.25],
		};
		let bytes = encode(&v).unwrap();
		let back: Sample = decode(&bytes).unwrap();
		assert_eq!(v, back);
	}

	#[test]
	fn codec_prepends_format_version() {
		let bytes = encode(&Sample {
			name: "x".into(),
			nums: vec![],
		})
		.unwrap();
		assert_eq!(
			bytes[0], FORMAT_VERSION,
			"first byte is the current write version"
		);
	}

	#[test]
	fn decode_rejects_older_version_bytes() {
		let mut bytes = encode(&Sample {
			name: "x".into(),
			nums: vec![1.0],
		})
		.unwrap();
		bytes[0] = FORMAT_VERSION - 1;
		match decode::<Sample>(&bytes) {
			Err(StoreError::BadVersion(v)) => assert_eq!(v, FORMAT_VERSION - 1),
			other => panic!("expected BadVersion, got {other:?}"),
		}
	}

	// The `ReviewState` field made every v6 Entity row mis-decode positionally, so
	// the bump has to reject a v6 store outright rather than read one field short.
	// Pinned on the literal 6, not on `FORMAT_VERSION - 1`: the next bump must not
	// silently drag this assertion forward and stop covering this one.
	#[test]
	fn a_v6_entity_row_is_rejected_rather_than_misdecoded() {
		let e = mk_entity("v6", "written before review states", 0.5, EntityKind::Claim);
		let mut bytes = encode_at(6, &e).unwrap();
		match decode::<Entity>(&bytes) {
			Err(StoreError::BadVersion(v)) => assert_eq!(v, 6),
			other => panic!("expected BadVersion(6), got {other:?}"),
		}
		bytes[0] = FORMAT_VERSION;
		assert_eq!(
			decode::<Entity>(&bytes).unwrap().review,
			ReviewState::Active,
			"the same bytes at the live version still decode — 6 is refused for its VERSION, not its shape"
		);
	}

	#[test]
	fn decode_rejects_unknown_version() {
		let mut bytes = encode(&Sample {
			name: "x".into(),
			nums: vec![1.0],
		})
		.unwrap();
		bytes[0] = 0xFF;
		match decode::<Sample>(&bytes) {
			Err(StoreError::BadVersion(0xFF)) => {}
			other => panic!("expected BadVersion(0xFF), got {other:?}"),
		}
	}

	#[test]
	fn put_get_remove_roundtrip() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let v = Sample {
			name: "k".into(),
			nums: vec![0.1, 0.2],
		};
		s.put(s.kern, "k", &v).unwrap();
		assert_eq!(s.get::<Sample>(s.kern, "k").unwrap(), Some(v));
		s.remove(s.kern, "k").unwrap();
		assert_eq!(s.get::<Sample>(s.kern, "k").unwrap(), None);
	}

	#[test]
	fn get_absent_is_none() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		assert_eq!(s.get::<Sample>(s.kern, "missing").unwrap(), None);
	}

	#[test]
	fn scan_returns_all_rows() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		for i in 0..5 {
			s.put(
				s.kern,
				&format!("k{i}"),
				&Sample {
					name: format!("n{i}"),
					nums: vec![i as f64],
				},
			)
			.unwrap();
		}
		let mut rows: Vec<(String, Sample)> = s.scan_with(s.kern, decode).unwrap();
		rows.sort_by(|a, b| a.0.cmp(&b.0));
		assert_eq!(rows.len(), 5);
		assert_eq!(rows[2].0, "k2");
		assert_eq!(rows[2].1.name, "n2");
	}

	#[test]
	fn reopen_persists_data() {
		let d = tmp();
		let dir = dir_of(&d);
		{
			let s = Store::open(&dir).unwrap();
			s.put(
				s.kern,
				"k",
				&Sample {
					name: "durable".into(),
					nums: vec![9.0],
				},
			)
			.unwrap();
		}
		let s2 = Store::open(&dir).unwrap();
		assert_eq!(
			s2.get::<Sample>(s2.kern, "k").unwrap().unwrap().name,
			"durable"
		);
	}

	fn kern_with(id: &str, entity: Entity) -> Kern {
		let mut k = Kern::new(id, "");
		k.entities.insert(entity.id.clone(), entity);
		k
	}

	// The append-only law covers the bytes, not the Rust type. `Entity::vector`
	// became `Arc<[f32]>` for ROADMAP item 83; serde encodes both a `Vec<T>` and
	// an `Arc<[T]>` as a plain seq, so the persisted layout is unmoved and no
	// format version turns. This is the guard on that claim.
	#[test]
	fn an_embedding_encodes_byte_identically_to_the_vec_it_replaced() {
		let v = vec![0.5f32, -0.25, 1.0, 0.0];
		let as_vec = bincode::serde::encode_to_vec(&v, bincode_cfg()).expect("vec encodes");
		let as_arc = bincode::serde::encode_to_vec(Embedding::from(v.clone()), bincode_cfg())
			.expect("arc encodes");
		assert_eq!(
			as_vec, as_arc,
			"Arc<[f32]> must occupy the same bincode bytes a Vec<f32> did"
		);
		let (back, _) = bincode::serde::decode_from_slice::<Embedding, _>(&as_vec, bincode_cfg())
			.expect("vec-written bytes decode as an Embedding");
		assert_eq!(&back[..], &v[..], "round trip is lossless");
	}

	#[test]
	fn stored_kern_roundtrip_quantizes_and_drops_gnn() {
		let mut e = mk_entity("e1", "a fact", 1.0, EntityKind::Claim);
		e.vector = vec![0.1, -0.2, 0.3, 0.4].into();
		e.gnn_vector = vec![1.0, 1.0, 1.0, 1.0].into();
		let k = kern_with("k", e);

		let back = StoredKern::from_kern(&k).into_kern();
		let be = &back.entities["e1"];
		assert_eq!(be.vector.len(), 4, "vector recovered");
		for (got, want) in be.vector.iter().zip([0.1, -0.2, 0.3, 0.4]) {
			assert!(
				(got - want).abs() < 0.02,
				"int8 within tolerance: {got} vs {want}"
			);
		}
		assert!(
			be.gnn_vector.is_empty(),
			"gnn_vector is dropped, not persisted"
		);
		assert_eq!(be.heat, 1.0, "non-vector fields survive");
		assert_eq!(be.text(), "a fact");
	}

	#[test]
	fn stored_kern_handles_empty_vectors() {
		let e = mk_entity("e1", "novec", 0.0, EntityKind::Claim);
		let mut e = e;
		e.vector = Embedding::default();
		let k = kern_with("k", e);
		let sk = StoredKern::from_kern(&k);
		assert!(
			sk.entity_vecs.is_empty(),
			"no side-map entry for an empty vector"
		);
		let back = sk.into_kern();
		assert!(!back.entities["e1"].has_vector());
	}

	#[test]
	fn stored_kern_roundtrips_temporal_stamps() {
		let t0 = UNIX_EPOCH + Duration::from_secs(1000);
		let t1 = UNIX_EPOCH + Duration::from_secs(2000);
		let mut e = mk_entity("e1", "a claim", 1.0, EntityKind::Claim);
		e.vector = vec![0.1, 0.2].into();
		e.valid_from = Some(t0);
		e.valid_to = Some(t1);
		e.invalidated_at = Some(t1);
		let k = kern_with("k", e);

		let bytes = encode(&StoredKern::from_kern(&k)).unwrap();
		assert_eq!(bytes[0], FORMAT_VERSION, "kern rows carry the live version");
		let back = decode::<StoredKern>(&bytes).unwrap().into_kern();
		let be = &back.entities["e1"];
		assert_eq!(be.valid_from, Some(t0), "valid_from survives");
		assert_eq!(be.valid_to, Some(t1), "valid_to survives");
		assert_eq!(be.invalidated_at, Some(t1), "invalidated_at survives");
	}

	#[test]
	fn save_then_load_graph_roundtrip() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let mut e = mk_entity("e1", "hello", 2.0, EntityKind::Fact);
		e.vector = vec![0.5, -0.5, 0.25].into();
		let mut kerns = HashMap::new();
		kerns.insert("root".to_string(), Kern::new("root", ""));
		kerns.insert("k".to_string(), kern_with("k", e));

		s.save_all_kerns(
			&kerns,
			"net-123",
			QuantizationMode::Int8,
			&std::collections::HashSet::new(),
		)
		.unwrap();
		let (loaded, net, qm) = s.load_all_kerns().unwrap();

		assert_eq!(net, "net-123");
		assert_eq!(qm, QuantizationMode::Int8);
		assert_eq!(loaded.len(), 2);
		let be = &loaded["k"].entities["e1"];
		assert_eq!(be.text(), "hello");
		assert!((be.vector[0] - 0.5).abs() < 0.02);
	}

	#[test]
	fn epoch_starts_at_zero_and_every_full_save_bumps_it() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		assert_eq!(s.read_epoch(), 0, "a fresh store reads epoch 0");
		let mut kerns = HashMap::new();
		kerns.insert("root".to_string(), Kern::new("root", ""));
		assert_eq!(
			s.save_all_kerns(
				&kerns,
				"n",
				QuantizationMode::None,
				&std::collections::HashSet::new()
			)
			.unwrap(),
			1,
			"first save bumps to 1"
		);
		assert_eq!(s.read_epoch(), 1);
		assert_eq!(
			s.save_all_kerns(
				&kerns,
				"n",
				QuantizationMode::None,
				&std::collections::HashSet::new()
			)
			.unwrap(),
			2,
			"each save advances the epoch"
		);
		assert_eq!(s.read_epoch(), 2);
	}

	#[test]
	fn flush_guarded_refuses_a_stale_smaller_snapshot_and_keeps_disk_rows() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();

		let mut a = HashMap::new();
		a.insert("root".to_string(), Kern::new("root", ""));
		a.insert(
			"ka".to_string(),
			kern_with("ka", mk_entity("ea", "durable", 1.0, EntityKind::Claim)),
		);
		assert_eq!(
			s.flush_guarded(
				&a,
				"n",
				QuantizationMode::None,
				0,
				&std::collections::HashSet::new()
			)
			.unwrap(),
			FlushOutcome::Flushed { epoch: 1 },
		);

		let mut b = HashMap::new();
		b.insert("root".to_string(), Kern::new("root", ""));
		assert_eq!(
			s.flush_guarded(
				&b,
				"n",
				QuantizationMode::None,
				0,
				&std::collections::HashSet::new()
			)
			.unwrap(),
			FlushOutcome::RefusedStale {
				disk_epoch: 1,
				expected: 0,
			},
		);
		let (loaded, _, _) = s.load_all_kerns().unwrap();
		assert!(
			loaded.contains_key("ka"),
			"a refused stale flush must not drop the row another writer committed"
		);
		assert_eq!(s.read_epoch(), 1, "a refusal does not advance the epoch");

		assert_eq!(
			s.flush_guarded(
				&b,
				"n",
				QuantizationMode::None,
				1,
				&std::collections::HashSet::new()
			)
			.unwrap(),
			FlushOutcome::Flushed { epoch: 2 },
		);
		let (loaded, _, _) = s.load_all_kerns().unwrap();
		assert!(
			!loaded.contains_key("ka"),
			"an up-to-date flush still prunes removed rows"
		);
	}

	#[test]
	fn save_all_prunes_removed_kerns() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let mut kerns = HashMap::new();
		kerns.insert("a".to_string(), Kern::new("a", ""));
		kerns.insert("b".to_string(), Kern::new("b", ""));
		s.save_all_kerns(
			&kerns,
			"n",
			QuantizationMode::None,
			&std::collections::HashSet::new(),
		)
		.unwrap();

		kerns.remove("b");
		s.save_all_kerns(
			&kerns,
			"n",
			QuantizationMode::None,
			&std::collections::HashSet::new(),
		)
		.unwrap();

		let (loaded, _, _) = s.load_all_kerns().unwrap();
		assert!(loaded.contains_key("a"));
		assert!(!loaded.contains_key("b"), "removed kern pruned from disk");
	}

	#[test]
	fn the_prune_spares_rows_named_in_spare() {
		// Regression for the idle-unload wipe: an unloaded kern's disk row is the
		// only copy of its thoughts, and the flush prune used to delete it because
		// the resident map no longer named it.
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		s.save_one_kern(&kern_with(
			"unloaded",
			mk_entity("e", "kept", 0.5, EntityKind::Claim),
		))
		.unwrap();

		let mut kerns = HashMap::new();
		kerns.insert("root".to_string(), Kern::new("root", ""));
		let mut spare = std::collections::HashSet::new();
		spare.insert("unloaded".to_string());
		s.save_all_kerns(&kerns, "n", QuantizationMode::None, &spare)
			.unwrap();
		assert!(
			s.load_one_kern("unloaded").unwrap().is_some(),
			"a spared row survives the prune"
		);

		s.save_all_kerns(
			&kerns,
			"n",
			QuantizationMode::None,
			&std::collections::HashSet::new(),
		)
		.unwrap();
		assert!(
			s.load_one_kern("unloaded").unwrap().is_none(),
			"without spare the prune reclaims it — deregistered kerns must not resurrect"
		);
	}

	#[test]
	fn compact_dir_skips_a_store_below_the_min_size() {
		let d = tmp();
		let dir = dir_of(&d);
		{
			let s = Store::open(&dir).unwrap();
			let mut kerns = HashMap::new();
			kerns.insert("root".to_string(), Kern::new("root", ""));
			s.save_all_kerns(
				&kerns,
				"n",
				QuantizationMode::Int8,
				&std::collections::HashSet::new(),
			)
			.unwrap();
			assert!(
				s.data_file_len() < crate::base::constants::COLD_COMPACT_MIN_BYTES,
				"tiny store"
			);
		}

		let (old_len, new_len) = compact_dir(&dir).unwrap();
		assert_eq!(
			old_len, new_len,
			"under the threshold compaction is a no-op"
		);
		assert!(
			!compact_tmp(Path::new(&dir)).exists(),
			"no env copy was made"
		);

		let s2 = Store::open(&dir).unwrap();
		assert!(s2.load_all_kerns().unwrap().0.contains_key("root"));
	}

	#[test]
	fn compact_dir_shrinks_after_bulk_delete() {
		let d = tmp();
		let dir = dir_of(&d);
		{
			let s = Store::open(&dir).unwrap();
			let mut kerns = HashMap::new();
			kerns.insert("root".to_string(), Kern::new("root", ""));
			for i in 0..2000 {
				let mut e = mk_entity(&format!("e{i}"), &"x".repeat(512), 1.0, EntityKind::Claim);
				e.vector = (0..256).map(|j| ((i + j) as f64).sin() as f32).collect();
				kerns.insert(format!("k{i}"), kern_with(&format!("k{i}"), e));
			}
			s.save_all_kerns(
				&kerns,
				"n",
				QuantizationMode::Int8,
				&std::collections::HashSet::new(),
			)
			.unwrap();
			let bloated = s.data_file_len();

			let mut small = HashMap::new();
			small.insert("root".to_string(), Kern::new("root", ""));
			small.insert("k0".to_string(), kerns.remove("k0").unwrap());
			s.save_all_kerns(
				&small,
				"n",
				QuantizationMode::Int8,
				&std::collections::HashSet::new(),
			)
			.unwrap();
			assert!(
				s.data_file_len() >= bloated * 9 / 10,
				"LMDB keeps ~all of the high-water mark after delete: {} vs peak {bloated}",
				s.data_file_len(),
			);
		} // env dropped so the offline compactor can swap the file

		let (old_len, new_len) = compact_dir(&dir).unwrap();
		assert!(
			new_len < old_len,
			"compaction shrinks the file: {old_len} -> {new_len}"
		);

		let s2 = Store::open(&dir).unwrap();
		let (loaded, _, _) = s2.load_all_kerns().unwrap();
		assert!(loaded.contains_key("root"), "root survives compaction");
		assert!(loaded.contains_key("k0"), "survivor survives compaction");
		assert_eq!(
			loaded.len(),
			2,
			"only the live rows remain after compaction"
		);
	}

	#[test]
	fn single_kern_save_load_delete() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let k = kern_with("k", mk_entity("e1", "x", 0.0, EntityKind::Claim));
		s.save_one_kern(&k).unwrap();
		assert!(s.load_one_kern("k").unwrap().is_some());
		s.delete_one_kern("k").unwrap();
		assert!(s.load_one_kern("k").unwrap().is_none());
		s.delete_one_kern("k").unwrap();
	}

	#[test]
	fn corrupt_kern_value_is_skipped() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		s.save_one_kern(&kern_with(
			"good",
			mk_entity("e", "ok", 0.0, EntityKind::Claim),
		))
		.unwrap();
		{
			let mut wtxn = s.env.write_txn().unwrap();
			s.kern
				.put(&mut wtxn, "bad", b"not a valid value".as_slice())
				.unwrap();
			wtxn.commit().unwrap();
		}
		let (loaded, _, _) = s.load_all_kerns().unwrap();
		assert!(loaded.contains_key("good"), "valid kern loads");
		assert!(
			!loaded.contains_key("bad"),
			"corrupt kern skipped, not fatal"
		);
	}

	#[test]
	fn cold_spill_then_get() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let e = mk_entity("a", "hello cold", 0.0, EntityKind::Claim);
		s.cold_spill(&e).unwrap();
		let got = s.cold_get("a").unwrap().unwrap();
		assert_eq!(got.text(), "hello cold");
	}

	#[test]
	fn cold_latest_wins() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		s.cold_spill(&mk_entity("x", "v1", 1.0, EntityKind::Claim))
			.unwrap();
		s.cold_spill(&mk_entity("x", "v2", 5.0, EntityKind::Claim))
			.unwrap();
		let got = s.cold_get("x").unwrap().unwrap();
		assert_eq!(got.heat, 5.0, "a put overwrites — latest wins, no dup rows");
	}

	#[test]
	fn cold_search_ranks_by_cosine() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let mut ex = mk_entity("ex", "x axis", 0.0, EntityKind::Claim);
		ex.vector = vec![1.0, 0.0].into();
		let mut ey = mk_entity("ey", "y axis", 0.0, EntityKind::Claim);
		ey.vector = vec![0.0, 1.0].into();
		let mut enear = mk_entity("enear", "near x", 0.0, EntityKind::Claim);
		enear.vector = vec![0.9, 0.1].into();
		s.cold_spill(&ex).unwrap();
		s.cold_spill(&ey).unwrap();
		s.cold_spill(&enear).unwrap();

		let hits = s.cold_search(&[1.0, 0.0], 2).unwrap();
		assert_eq!(hits.len(), 2);
		assert_eq!(hits[0].0.id, "ex", "closest to query ranks first");
		// dimension mismatch yields nothing
		assert!(s.cold_search(&[1.0, 0.0, 0.0], 2).unwrap().is_empty());
	}

	// (id, score, vector) — the vector is compared too, because a split tier can
	// select the right rows and still hand back rows the caller cannot use.
	type ColdHit = (String, f64, Embedding);

	// The linear scan `cold_search` replaced: decode every row, score it, sort.
	// Kept here as the reference oracle — an index that selects different rows
	// than this is a recall change, not a speedup.
	fn cold_search_linear(s: &Store, q: &[f32], k: usize) -> Vec<ColdHit> {
		if q.is_empty() || k == 0 {
			return Vec::new();
		}
		let mut scored: Vec<ColdHit> = s
			.cold_all()
			.unwrap()
			.into_iter()
			.filter_map(|e| {
				if e.vector.len() != q.len() {
					return None;
				}
				let sc = crate::base::math::cosine(q, &e.vector);
				if sc.is_finite() {
					Some((e.id, sc, e.vector))
				} else {
					None
				}
			})
			.collect();
		// Reversed before sorting so the reference never inherits LMDB key order:
		// it has to reach id-ascending through the tie-break, not through the scan.
		scored.reverse();
		scored.sort_by(|a, b| crate::base::util::cmp_rank(a.1, &a.0, b.1, &b.0));
		scored.truncate(k);
		scored
	}

	#[test]
	fn cold_search_selects_exactly_what_the_linear_scan_did() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		const DIM: usize = 12;
		const N: usize = 600;
		let mut h: u64 = 0x243f_6a88_85a3_08d3;
		let mut next = || {
			h ^= h << 13;
			h ^= h >> 7;
			h ^= h << 17;
			h
		};
		let mut batch = Vec::new();
		for i in 0..N {
			let mut e = mk_entity(&format!("e{i:04}"), "cold row", 0.0, EntityKind::Claim);
			e.vector = match i % 20 {
				// Wrong dimension and empty vectors must be skipped by both paths.
				7 => vec![0.5; DIM + 1].into(),
				11 => Embedding::default(),
				// A repeated vector forces exact cosine ties, so the id tie-break is
				// exercised rather than assumed.
				13 => vec![1.0; DIM].into(),
				_ => (0..DIM)
					.map(|_| (next() % 2000) as f32 / 1000.0 - 1.0)
					.collect(),
			};
			batch.push(e);
		}
		s.cold_put_all(&batch).unwrap();

		for qi in 0..8 {
			let q: Vec<f32> = (0..DIM)
				.map(|_| (next() % 2000) as f32 / 1000.0 - 1.0)
				.collect();
			for k in [1usize, 5, 15, 100, N] {
				let want = cold_search_linear(&s, &q, k);
				let got: Vec<ColdHit> = s
					.cold_search(&q, k)
					.unwrap()
					.into_iter()
					.map(|(e, sc)| (e.id, sc, e.vector))
					.collect();
				assert_eq!(
					got, want,
					"query {qi} k={k}: indexed cold_search must select the same rows, \
					 in the same order, with the same scores as the linear scan"
				);
			}
		}
		// An all-ties query would leave ordering to the tie-break alone; here only
		// the repeated-vector rows tie, which is enough to pin it.
		let tie_q = vec![1.0f32; DIM];
		let want = cold_search_linear(&s, &tie_q, 40);
		let got: Vec<ColdHit> = s
			.cold_search(&tie_q, 40)
			.unwrap()
			.into_iter()
			.map(|(e, sc)| (e.id, sc, e.vector))
			.collect();
		assert_eq!(got, want, "tie-heavy query must break ties identically");
		assert!(
			want.windows(2).any(|w| w[0].1 == w[1].1),
			"the fixture must actually produce ties or it proves nothing"
		);
	}

	#[test]
	fn cold_cap_drops_oldest() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		for (i, id) in ["old", "mid", "new"].iter().enumerate() {
			let mut e = mk_entity(id, id, 1.0, EntityKind::Claim);
			e.created_at = Some(UNIX_EPOCH + Duration::from_secs(100 * (i as u64 + 1)));
			s.cold_spill(&e).unwrap();
		}
		s.cold_cap(2).unwrap();
		assert!(s.cold_get("new").unwrap().is_some(), "newest kept");
		assert!(s.cold_get("mid").unwrap().is_some(), "second-newest kept");
		assert!(s.cold_get("old").unwrap().is_none(), "oldest evicted");
		// An orphaned vector is invisible to cold_get but still scored by
		// cold_search, where it silently consumes one of the k slots.
		let rtxn = s.env.read_txn().unwrap();
		assert!(
			s.cold_vec.get(&rtxn, "old").unwrap().is_none(),
			"the evicted row's vector is evicted with it, not orphaned in the side table"
		);
	}

	#[test]
	fn cold_spill_round_trips_the_temporal_triple() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let t0 = UNIX_EPOCH + Duration::from_secs(1000);
		let t1 = UNIX_EPOCH + Duration::from_secs(2000);
		let mut e = mk_entity("a", "revision", 0.0, EntityKind::Claim);
		e.created_at = Some(t0);
		e.valid_from = Some(t0);
		e.valid_to = Some(t1);
		e.invalidated_at = Some(t1);
		s.cold_spill(&e).unwrap();

		let got = s.cold_get("a").unwrap().unwrap();
		assert_eq!(
			got.valid_from,
			Some(t0),
			"valid_from survives the cold tier"
		);
		assert_eq!(got.valid_to, Some(t1), "valid_to survives the cold tier");
		assert_eq!(got.invalidated_at, Some(t1), "invalidated_at survives");
		assert!(got.is_valid_at(t0), "valid at the window start");
		assert!(
			!got.is_valid_at(t1),
			"half-open window: not valid at valid_to"
		);
		assert!(
			!got.is_valid_at(UNIX_EPOCH),
			"not valid before valid_from — as_of must not lie over the cold tail"
		);
		assert_eq!(s.cold_all().unwrap()[0].valid_to, Some(t1));
	}

	#[test]
	fn a_cold_row_missing_its_tail_is_never_decoded_as_a_stampless_entity() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let mut e = mk_entity("trunc", "lost its tail", 0.0, EntityKind::Claim);
		e.valid_from = Some(UNIX_EPOCH + Duration::from_secs(10));
		// A current-version value whose ColdRow tail is missing: a bare Entity
		// parses cleanly as a ColdRow prefix, so only a strict decode catches it.
		{
			let bytes = encode_at(FORMAT_VERSION, &e).unwrap();
			let mut wtxn = s.env.write_txn().unwrap();
			s.cold.put(&mut wtxn, "trunc", bytes.as_slice()).unwrap();
			wtxn.commit().unwrap();
		}
		assert!(
			s.cold_get("trunc").is_err(),
			"a malformed current-version row must ERROR, not silently lose its stamps"
		);
		assert!(
			s.cold_all().unwrap().is_empty(),
			"scan_with's corruption warning fires because the decode really failed"
		);

		// Round-trip through the real writer keeps the stamps.
		s.cold_spill(&e).unwrap();
		assert_eq!(
			s.cold_get("trunc").unwrap().unwrap().valid_from,
			e.valid_from
		);
	}

	#[test]
	fn cold_cap_counts_every_eviction() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		assert_eq!(s.cold_evicted(), 0, "nothing evicted yet");
		for (i, id) in ["old", "mid", "new"].iter().enumerate() {
			let mut e = mk_entity(id, id, 1.0, EntityKind::Claim);
			e.created_at = Some(UNIX_EPOCH + Duration::from_secs(100 * (i as u64 + 1)));
			s.cold_spill(&e).unwrap();
		}
		s.cold_cap(3).unwrap();
		assert_eq!(s.cold_evicted(), 0, "a cap that fits evicts nothing");
		s.cold_cap(2).unwrap();
		assert_eq!(s.cold_evicted(), 1, "one row dropped is one counted");
		s.cold_cap(0).unwrap();
		assert_eq!(s.cold_evicted(), 3, "the counter accumulates, never resets");
	}

	#[test]
	fn a_cold_tier_pinned_at_capacity_counts_every_eviction_but_logs_once() {
		use std::sync::atomic::{AtomicUsize, Ordering};
		use std::sync::Arc;
		use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

		struct CountWarns(Arc<AtomicUsize>);
		impl<S: tracing::Subscriber> Layer<S> for CountWarns {
			fn on_event(&self, e: &tracing::Event<'_>, _: Context<'_, S>) {
				if *e.metadata().level() == tracing::Level::WARN {
					self.0.fetch_add(1, Ordering::Relaxed);
				}
			}
		}

		let warns = Arc::new(AtomicUsize::new(0));
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		// A tier at max evicts on EVERY spill, and one GC sweep spills once per
		// victim — the pre-fix code emitted one warn line per victim, forever.
		tracing::subscriber::with_default(
			tracing_subscriber::registry().with(CountWarns(warns.clone())),
			|| {
				for i in 0..50u64 {
					let mut e = mk_entity(&format!("e{i}"), "x", 1.0, EntityKind::Claim);
					e.created_at = Some(UNIX_EPOCH + Duration::from_secs(i + 1));
					s.cold_spill(&e).unwrap();
					s.cold_cap(1).unwrap();
				}
			},
		);

		assert_eq!(s.cold_evicted(), 49, "every eviction reaches the counter");
		assert_eq!(
			warns.load(Ordering::Relaxed),
			1,
			"49 evictions produce ONE log line, not 49"
		);
	}

	fn stamp(model: &str, dim: usize) -> EmbedStamp {
		EmbedStamp {
			model: model.into(),
			dim,
		}
	}

	#[test]
	fn an_unstamped_store_adopts_the_current_embed_model() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		assert_eq!(s.embed_stamp(), None, "an unstamped store reads None");
		assert_eq!(
			s.check_embed_stamp(&stamp("qwen3", 1024)).unwrap(),
			EmbedCheck::Adopted,
			"unknown adopts, never accuses"
		);
		assert_eq!(s.embed_stamp(), Some(stamp("qwen3", 1024)), "stamp written");
		assert!(!s.embed_mismatch());
		assert_eq!(
			s.check_embed_stamp(&stamp("qwen3", 1024)).unwrap(),
			EmbedCheck::Match,
			"the adopted stamp is silent on the next open"
		);
		assert!(!s.embed_mismatch());
	}

	#[test]
	fn a_changed_embed_model_or_dimension_is_flagged() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		s.set_embed_stamp(&stamp("qwen3", 1024)).unwrap();

		assert_eq!(
			s.check_embed_stamp(&stamp("nomic", 1024)).unwrap(),
			EmbedCheck::Mismatch {
				stored: stamp("qwen3", 1024),
				current: stamp("nomic", 1024),
			},
			"a different model name is a mismatch at the same dimension"
		);
		assert!(s.embed_mismatch(), "the mismatch is recorded for health");
		assert_eq!(
			s.embed_stamp(),
			Some(stamp("qwen3", 1024)),
			"the disk stamp still describes what is stored"
		);

		s.set_embed_stamp(&stamp("qwen3", 1024)).unwrap();
		assert!(!s.embed_mismatch(), "restamping clears the flag");
		assert_eq!(
			s.check_embed_stamp(&stamp("qwen3", 768)).unwrap(),
			EmbedCheck::Mismatch {
				stored: stamp("qwen3", 1024),
				current: stamp("qwen3", 768),
			},
			"a different dimension is a mismatch at the same model name"
		);
		assert!(s.embed_mismatch());

		assert_eq!(
			s.check_embed_stamp(&stamp("qwen3", 1024)).unwrap(),
			EmbedCheck::Match,
			"reverting to the stored model matches again"
		);
		assert!(
			!s.embed_mismatch(),
			"health must stop accusing once the operator has reverted"
		);
	}

	#[test]
	fn the_embed_stamp_survives_reopen() {
		let d = tmp();
		let dir = dir_of(&d);
		{
			let s = Store::open(&dir).unwrap();
			s.check_embed_stamp(&stamp("qwen3", 1024)).unwrap();
		}
		let s2 = Store::open(&dir).unwrap();
		assert_eq!(
			s2.check_embed_stamp(&stamp("qwen3", 1024)).unwrap(),
			EmbedCheck::Match,
			"a stamp adopted in one process is durable"
		);
	}

	#[test]
	fn cold_search_breaks_cosine_ties_by_id_ascending() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		// Identical vectors; spill the higher id first so only the id tiebreak (not
		// scan/insert order) can pick the survivor of `truncate(1)`.
		let mut eb = mk_entity("b", "dup", 0.0, EntityKind::Claim);
		eb.vector = vec![1.0, 0.0].into();
		let mut ea = mk_entity("a", "dup", 0.0, EntityKind::Claim);
		ea.vector = vec![1.0, 0.0].into();
		s.cold_spill(&eb).unwrap();
		s.cold_spill(&ea).unwrap();

		let hits = s.cold_search(&[1.0, 0.0], 1).unwrap();
		assert_eq!(hits.len(), 1);
		assert_eq!(
			hits[0].0.id, "a",
			"equal-cosine tie resolved to id-ascending winner"
		);
	}
	#[test]
	fn a_spill_past_the_cap_does_not_trim_until_the_slack_is_used_up() {
		// The cliff this closes: cold_cap decodes every row to sort by age, so
		// calling it per spill made one GC sweep pay a full-table pass per victim.
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		let max = 4usize;

		for i in 0..(max + 3) {
			let mut e = mk_entity(&format!("e{i}"), "x", 1.0, EntityKind::Claim);
			e.created_at = Some(UNIX_EPOCH + Duration::from_secs(100 * (i as u64 + 1)));
			s.cold_spill(&e).unwrap();
			s.cold_cap_amortized(max).unwrap();
		}

		assert_eq!(
			s.cold_evicted(),
			0,
			"inside the slack margin nothing is trimmed, so nothing is decoded"
		);
		assert!(
			s.cold_get("e0").unwrap().is_some(),
			"the oldest row is still present while under max + slack"
		);
	}

	#[test]
	fn an_amortized_trim_cuts_all_the_way_back_to_the_cap() {
		let d = tmp();
		let s = Store::open(&dir_of(&d)).unwrap();
		for i in 0..3 {
			let mut e = mk_entity(&format!("e{i}"), "x", 1.0, EntityKind::Claim);
			e.created_at = Some(UNIX_EPOCH + Duration::from_secs(100 * (i as u64 + 1)));
			s.cold_spill(&e).unwrap();
		}
		// max 0 forces the trigger regardless of slack, since 3 > 0 + SLACK is false
		// — so drive the exact-cap path the amortized one delegates to.
		s.cold_cap(1).unwrap();
		assert_eq!(s.cold_evicted(), 2, "trims to the cap, not to the trigger");
		assert!(s.cold_get("e2").unwrap().is_some(), "newest survives");
	}
}
