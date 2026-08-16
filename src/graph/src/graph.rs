//! The resident graph. [`GraphGnn`] owns every loaded [`Kern`], the ANN and
//! lexical indices over their entities, the source/entity/reason routing maps,
//! and the LMDB store handle — one instance per data dir, shared behind an
//! `RwLock`. Mutation policy (accept, supersede, merge) lives in `accept`/
//! `reason`/`merge`; this file holds the structure, caches, and load/unload
//! mechanics they operate on.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use super::diskann::DiskIndex;
use super::lexical::LexicalIndex;
use super::vector_backend::VectorBackend;
use base::base_constants::KERN_CAP_DISABLED;
use base::base_types::{EntityStatus, Kern};
use math::quant::QuantizationMode;
use store_core::{Store, StoreError};

#[allow(clippy::too_many_arguments)]
fn index_kern_into(
	kern: &Kern,
	entity_kern: &mut HashMap<String, String>,
	reason_kern: &mut HashMap<String, String>,
	src_index: &mut HashMap<String, String>,
	// `None` skips vector inserts: a disk snapshot ALREADY holds every resident
	// vector — re-inserting would tombstone it all into the delta.
	mut entity_idx: Option<&mut VectorBackend>,
	mut gnn_entity_idx: Option<&mut VectorBackend>,
	mut reason_idx: Option<&mut VectorBackend>,
) {
	// HNSW structure depends on insert order — populate in id order, never HashMap
	// order (differs per process).
	let mut entities: Vec<_> = kern.entities.values().collect();
	entities.sort_by(|a, b| a.id.cmp(&b.id));
	for t in entities {
		entity_kern.insert(t.id.clone(), kern.id.clone());
		let searchable = t.status != EntityStatus::Superseded;
		if searchable && t.has_vector() {
			if let Some(ei) = entity_idx.as_deref_mut() {
				ei.insert(t.id.clone(), t.vector.clone());
			}
		}
		if searchable && t.has_gnn_vector() {
			if let Some(gi) = gnn_entity_idx.as_deref_mut() {
				gi.insert(t.id.clone(), t.gnn_vector.clone());
			}
		}
	}
	let mut reasons: Vec<_> = kern.reasons.values().collect();
	reasons.sort_by(|a, b| a.id.cmp(&b.id));
	for r in reasons {
		reason_kern.insert(r.id.clone(), kern.id.clone());
		if r.has_vector() {
			if let Some(ri) = reason_idx.as_deref_mut() {
				ri.insert(r.id.clone(), r.vector.clone());
			}
		}
	}
	for ext_id in kern.source_index.keys() {
		src_index.insert(ext_id.clone(), kern.id.clone());
	}
}

// Fold a stale snapshot's diff into the delta overlay (RECALL_PLAN F4): tombstone
// snapshot ids the graph no longer has, and delta-insert ids whose vector is
// missing or changed. When the diff has outgrown the snapshot, a full rebuild is
// cheaper than an ever-growing delta — amortized doubling.
enum IndexKind {
	Entity,
	Gnn,
	Reason,
}

impl IndexKind {
	fn subdir(&self) -> &'static str {
		match self {
			IndexKind::Entity => "entity",
			IndexKind::Gnn => "gnn",
			IndexKind::Reason => "reason",
		}
	}
}

impl GraphGnn {
	fn reconcile_disk(&mut self, kind: IndexKind, items: &[(String, Vec<f32>)]) {
		let (to_delete, to_insert, snap_count) = {
			let backend = match kind {
				IndexKind::Entity => &self.entity_idx,
				IndexKind::Gnn => &self.gnn_entity_idx,
				IndexKind::Reason => &self.reason_idx,
			};
			let VectorBackend::Disk { snapshot, .. } = backend else {
				return;
			};
			let live: HashSet<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
			let mut to_delete = Vec::new();
			for id in snapshot.ids() {
				if !live.contains(id.as_str()) {
					to_delete.push(id.clone());
				}
			}
			let mut to_insert = Vec::new();
			for (id, vec) in items {
				match snapshot.vector_of(id) {
					Some(v) if v == *vec => {}
					_ => to_insert.push((id.clone(), vec.clone())),
				}
			}
			(to_delete, to_insert, snapshot.len())
		};
		let backend = match kind {
			IndexKind::Entity => &mut self.entity_idx,
			IndexKind::Gnn => &mut self.gnn_entity_idx,
			IndexKind::Reason => &mut self.reason_idx,
		};
		if to_insert.len() > snap_count {
			// The diff has outgrown the snapshot — a full rebuild folds everything.
			let qm = self.quant_mode;
			let built = self.build_disk_snapshot(kind.subdir(), items.to_vec());
			let backend = match kind {
				IndexKind::Entity => &mut self.entity_idx,
				IndexKind::Gnn => &mut self.gnn_entity_idx,
				IndexKind::Reason => &mut self.reason_idx,
			};
			*backend = GraphGnn::disk_or_resident(built, qm);
			return;
		}
		for id in to_delete {
			backend.delete(&id);
		}
		for (id, vec) in to_insert {
			backend.insert(id, vec.into());
		}
	}
}

pub struct GraphGnn {
	pub root: Kern,
	pub replica_id: String,
	pub data_dir: String,
	lamport: std::sync::atomic::AtomicU64,
	// Rephrase edges re-pointed at a supersede (the carrying entity was
	// superseded by a different update than the deferred candidate) awaiting
	// re-classification on the tick loop (ROADMAP item 60). (kern_id, reason_id).
	pending_reclass: parking_lot::Mutex<Vec<(String, String)>>,
	// LMDB forbids opening one env twice in a process; opened once and shared.
	store: Option<Arc<Store>>,
	pub quant_mode: QuantizationMode,
	pub gnn_entity_idx: VectorBackend,
	pub entity_idx: VectorBackend,
	pub reason_idx: VectorBackend,
	pub kerns: HashMap<String, Kern>,
	unloaded: HashSet<String>,
	src_index: HashMap<String, String>,
	entity_kern: HashMap<String, String>,
	reason_kern: HashMap<String, String>,
	lexical: Option<Arc<LexicalIndex>>,
	max_loaded_kerns: usize,
	disk_threshold: usize,
	// Must stay GLOBAL — the adjacency cache and the dirty-flush loops compare
	// one number for the whole graph; per-kern versions would miss cross-kern edits.
	mutation_epoch: u64,
	flushed_epoch: u64,
	adjacency_cache: parking_lot::RwLock<Option<(u64, Arc<EntityAdjacency>)>>,
	entity_dim_cache: parking_lot::RwLock<Option<usize>>,
	// The CONFIGURED embedding model, bound at open. Empty until a caller that has
	// a config binds it; the store stamp is only written once it is known.
	embed_model: String,
}

pub struct EntityAdjacency {
	pub id_to_idx: HashMap<String, usize>,
	pub ids: Vec<String>,
	pub out: Vec<Vec<usize>>,
}

impl EntityAdjacency {
	fn build(g: &GraphGnn) -> Self {
		let mut id_to_idx: HashMap<String, usize> = HashMap::new();
		let mut ids: Vec<String> = Vec::new();
		for kern in g.map().values() {
			for t in kern.entities.values() {
				if !id_to_idx.contains_key(&t.id) {
					id_to_idx.insert(t.id.clone(), ids.len());
					ids.push(t.id.clone());
				}
			}
		}
		let mut out: Vec<Vec<usize>> = vec![Vec::new(); ids.len()];
		for kern in g.map().values() {
			for r in kern.reasons.values() {
				if r.from == r.to {
					continue;
				}
				let (Some(&fi), Some(&ti)) = (id_to_idx.get(&r.from), id_to_idx.get(&r.to)) else {
					continue;
				};
				out[fi].push(ti);
			}
		}
		Self {
			id_to_idx,
			ids,
			out,
		}
	}
}

impl Default for GraphGnn {
	fn default() -> Self {
		Self::new()
	}
}

impl GraphGnn {
	pub fn new() -> Self {
		let mut root = Kern::new_root();
		let replica_id = util::uuid_v4();
		root.root_id = replica_id.clone();
		let root_id = root.id.clone();
		let mut kerns = HashMap::new();
		kerns.insert(root_id, root.clone());
		let quant_mode = QuantizationMode::default();
		Self {
			root,
			replica_id,
			data_dir: String::new(),
			lamport: std::sync::atomic::AtomicU64::new(0),
			pending_reclass: parking_lot::Mutex::new(Vec::new()),
			store: None,
			quant_mode,
			entity_idx: VectorBackend::resident(16, 200, quant_mode),
			gnn_entity_idx: VectorBackend::resident(16, 200, quant_mode),
			reason_idx: VectorBackend::resident(16, 200, quant_mode),
			kerns,
			unloaded: HashSet::new(),
			src_index: HashMap::new(),
			entity_kern: HashMap::new(),
			reason_kern: HashMap::new(),
			lexical: Some(Arc::new(LexicalIndex::new_in_ram(1.2, 0.75))),
			max_loaded_kerns: KERN_CAP_DISABLED,
			disk_threshold: KERN_CAP_DISABLED,
			mutation_epoch: 0,
			flushed_epoch: 0,
			adjacency_cache: parking_lot::RwLock::new(None),
			entity_dim_cache: parking_lot::RwLock::new(None),
			embed_model: String::new(),
		}
	}

	pub fn flushed_epoch(&self) -> u64 {
		self.flushed_epoch
	}

	// Not a content mutation — must NOT bump mutation_epoch.
	pub fn set_flushed_epoch(&mut self, epoch: u64) {
		self.flushed_epoch = epoch;
	}

	pub fn set_max_loaded_kerns(&mut self, cap: usize) {
		self.max_loaded_kerns = cap.max(1);
	}

	/// The resident-kern cap (`KERN_CAP_DISABLED` = uncapped). Armed via
	/// `set_max_loaded_kerns` / `apply_graph_config`; surfaced in health (item 83).
	pub fn max_loaded_kerns(&self) -> usize {
		self.max_loaded_kerns
	}

	pub fn set_disk_threshold(&mut self, threshold: usize) {
		self.disk_threshold = threshold;
	}

	pub fn set_store(&mut self, store: Arc<Store>) {
		self.store = Some(store);
	}

	pub fn set_embed_model(&mut self, model: &str) {
		self.embed_model = model.to_string();
	}

	pub fn embed_model(&self) -> &str {
		&self.embed_model
	}

	pub fn store(&self) -> Option<Arc<Store>> {
		self.store.clone()
	}

	fn enforce_kern_cap(&mut self) {
		if self.max_loaded_kerns == KERN_CAP_DISABLED {
			return;
		}
		while self.kerns.len() > self.max_loaded_kerns {
			let root_id = self.root.id.clone();
			let victim = self
				.kerns
				.iter()
				.filter(|(id, _)| **id != root_id)
				.min_by_key(|(_, k)| k.last_access.unwrap_or(SystemTime::UNIX_EPOCH))
				.map(|(id, _)| id.clone());
			match victim {
				Some(id) => {
					let _ = self.unload(&id);
				}
				None => break,
			}
		}
	}

	pub fn lexical(&self) -> Option<Arc<LexicalIndex>> {
		self.lexical.clone()
	}

	// Length of the indexed entity vectors. Nothing enforces one dimension per
	// index, so the dominant length is the honest answer; ties break to the larger.
	// The filter MUST mirror index_kern_into — a dimension the index excludes would
	// reject every legitimate query on a supersede-heavy store.
	fn dominant_entity_dim(&self) -> Option<usize> {
		let mut counts: HashMap<usize, usize> = HashMap::new();
		for kern in self.kerns.values() {
			for t in kern.entities.values() {
				if t.status != EntityStatus::Superseded && t.has_vector() {
					*counts.entry(t.vector.len()).or_default() += 1;
				}
			}
		}
		counts
			.into_iter()
			.max_by_key(|&(dim, n)| (n, dim))
			.map(|(dim, _)| dim)
	}

	// ONE source of truth for both health and the query guard. The scan is
	// O(all entities), so it must not run per query: keying the memo on
	// mutation_epoch made it miss on every `get_mut`, and since accept_with_dedup
	// searches then commits, ingesting N entities into M cost N full walks.
	// An unknown answer is deliberately NOT cached — an empty store is cheap to
	// rescan, and caching None there would disable the guard for the daemon's life.
	pub fn entity_vector_dim(&self) -> Option<usize> {
		if let Some(dim) = *self.entity_dim_cache.read() {
			return Some(dim);
		}
		let dim = self.dominant_entity_dim();
		if let Some(d) = dim {
			*self.entity_dim_cache.write() = Some(d);
		}
		dim
	}

	// cosine() truncates to the shorter side, so a query from another embedding
	// model scores as noise instead of failing. Unknown never blocks.
	pub fn query_dim_ok(&self, query_vec: &[f32]) -> bool {
		match self.entity_vector_dim() {
			Some(dim) => query_vec.len() == dim,
			None => true,
		}
	}

	// A failed snapshot build lands here instead of silently shrinking the index.
	fn disk_or_resident(snapshot: Option<DiskIndex>, quant_mode: QuantizationMode) -> VectorBackend {
		match snapshot {
			Some(s) => VectorBackend::disk(s, quant_mode),
			None => VectorBackend::resident(16, 200, quant_mode),
		}
	}

	pub fn rebuild_index(&mut self) {
		// The one place the indexed dimension can change wholesale (reembed, load).
		*self.entity_dim_cache.write() = None;
		self.src_index.clear();
		self.entity_kern.clear();
		self.reason_kern.clear();

		let entity_count = self.resident_searchable_entity_count();
		let spill = !self.data_dir.is_empty() && entity_count > self.disk_threshold;
		let (e_items, g_items, r_items) = if spill {
			(
				self.collect_entity_items(),
				self.collect_gnn_items(),
				self.collect_reason_items(),
			)
		} else {
			(Vec::new(), Vec::new(), Vec::new())
		};
		let (e_fresh, g_fresh, r_fresh) = if spill {
			// Disk snapshots (mmap) for all three indexes. An unchanged store
			// (epoch stamp match) is a pure mmap; a changed store opens the old
			// snapshot and reconciles the diff into the delta overlay — the full
			// Vamana rebuild only runs on a missing index or when the diff has
			// outgrown the snapshot (amortized doubling). RECALL_PLAN F4.
			let (e, f) = self.open_snapshot("entity", &e_items);
			self.entity_idx = Self::disk_or_resident(e, self.quant_mode);
			let (g, gf) = self.open_snapshot("gnn", &g_items);
			self.gnn_entity_idx = Self::disk_or_resident(g, self.quant_mode);
			let (r, rf) = self.open_snapshot("reason", &r_items);
			self.reason_idx = Self::disk_or_resident(r, self.quant_mode);
			(f, gf, rf)
		} else {
			self.entity_idx = VectorBackend::resident(16, 200, self.quant_mode);
			self.gnn_entity_idx = VectorBackend::resident(16, 200, self.quant_mode);
			self.reason_idx = VectorBackend::resident(16, 200, self.quant_mode);
			(true, true, true)
		};

		// A disk snapshot already holds every resident vector — `None` skips the
		// re-insert (which would tombstone it) but still fills the reverse maps.
		let skip_entity_insert = matches!(self.entity_idx, VectorBackend::Disk { .. });
		let skip_gnn_insert = matches!(self.gnn_entity_idx, VectorBackend::Disk { .. });
		let skip_reason_insert = matches!(self.reason_idx, VectorBackend::Disk { .. });
		let mut kerns: Vec<&Kern> = self.kerns.values().collect();
		kerns.sort_by(|a, b| a.id.cmp(&b.id));
		for kern in kerns {
			index_kern_into(
				kern,
				&mut self.entity_kern,
				&mut self.reason_kern,
				&mut self.src_index,
				(!skip_entity_insert).then_some(&mut self.entity_idx),
				(!skip_gnn_insert).then_some(&mut self.gnn_entity_idx),
				(!skip_reason_insert).then_some(&mut self.reason_idx),
			);
		}

		// Fold the diff of a stale snapshot into the delta overlay: tombstone
		// removed ids, insert changed/new vectors. Fresh snapshots skip this.
		if !e_fresh && skip_entity_insert {
			self.reconcile_disk(IndexKind::Entity, &e_items);
		}
		if !g_fresh && skip_gnn_insert {
			self.reconcile_disk(IndexKind::Gnn, &g_items);
		}
		if !r_fresh && skip_reason_insert {
			self.reconcile_disk(IndexKind::Reason, &r_items);
		}
	}

	// Filter must mirror index_kern_into (drives the spill decision).
	fn resident_searchable_entity_count(&self) -> usize {
		self
			.kerns
			.values()
			.flat_map(|k| k.entities.values())
			.filter(|t| t.status != EntityStatus::Superseded && t.has_vector())
			.count()
	}

	// id-sorted (BTreeMap) so the Vamana build is reproducible.
	fn collect_entity_items(&self) -> Vec<(String, Vec<f32>)> {
		let mut items: std::collections::BTreeMap<String, Vec<f32>> = std::collections::BTreeMap::new();
		for kern in self.kerns.values() {
			for t in kern.entities.values() {
				if t.status != EntityStatus::Superseded && t.has_vector() {
					items.insert(t.id.clone(), t.vector.to_vec());
				}
			}
		}
		items.into_iter().collect()
	}

	// Mirror of `index_kern_into`'s gnn insert condition, so the snapshot and the
	// resident path index the same set.
	fn collect_gnn_items(&self) -> Vec<(String, Vec<f32>)> {
		let mut items: std::collections::BTreeMap<String, Vec<f32>> = std::collections::BTreeMap::new();
		for kern in self.kerns.values() {
			for t in kern.entities.values() {
				if t.status != EntityStatus::Superseded && t.has_gnn_vector() {
					items.insert(t.id.clone(), t.gnn_vector.to_vec());
				}
			}
		}
		items.into_iter().collect()
	}

	fn collect_reason_items(&self) -> Vec<(String, Vec<f32>)> {
		let mut items: std::collections::BTreeMap<String, Vec<f32>> = std::collections::BTreeMap::new();
		for kern in self.kerns.values() {
			for r in kern.reasons.values() {
				if r.has_vector() {
					items.insert(r.id.clone(), r.vector.to_vec());
				}
			}
		}
		items.into_iter().collect()
	}

	pub fn build_entity_disk_index(&self, dir: &std::path::Path) -> std::io::Result<usize> {
		super::diskann::build_and_save(
			dir,
			&self.collect_entity_items(),
			super::diskann::Params::default(),
		)
	}

	// Build (or reuse, when the epoch stamp matches) the Vamana snapshot for one
	// index and mmap it. None = fall back to the resident HNSW.
	fn build_disk_snapshot(&self, subdir: &str, items: Vec<(String, Vec<f32>)>) -> Option<DiskIndex> {
		let dir = Path::new(&self.data_dir).join("diskann").join(subdir);
		let epoch = self.store().as_ref().map(|s| s.read_epoch());
		if let Err(e) = super::diskann::build_and_save_with_epoch(
			&dir,
			&items,
			super::diskann::Params::default(),
			epoch,
		) {
			tracing::warn!(target: "kern.diskann", error = %e, subdir, "snapshot build failed; using in-RAM index");
			return None;
		}
		match super::diskann::DiskIndex::open(&dir) {
			Ok(idx) => Some(idx),
			Err(e) => {
				tracing::warn!(target: "kern.diskann", error = %e, subdir, "snapshot open failed; using in-RAM index");
				None
			}
		}
	}

	// Load path: prefer the existing snapshot over a rebuild. Returns
	// (index, fresh) where fresh means the snapshot already matches `items` and
	// needs no reconcile. Missing/corrupt index or a never-stamped dir falls
	// back to a full build (fresh by construction). A stale-but-valid snapshot
	// is OPENED as-is — the caller reconciles the diff into the delta, which is
	// O(changed) instead of a full Vamana build per write (RECALL_PLAN F4).
	fn open_snapshot(&self, subdir: &str, items: &[(String, Vec<f32>)]) -> (Option<DiskIndex>, bool) {
		let dir = Path::new(&self.data_dir).join("diskann").join(subdir);
		let fresh = match (
			self.store().as_ref().map(|s| s.read_epoch()),
			super::diskann::snapshot_epoch(&dir),
		) {
			(Some(e), Some(se)) => e == se,
			_ => false,
		};
		if fresh {
			match super::diskann::DiskIndex::open(&dir) {
				Ok(idx) => return (Some(idx), true),
				Err(e) => {
					tracing::warn!(target: "kern.diskann", error = %e, subdir, "fresh snapshot open failed; using in-RAM index");
					return (None, true);
				}
			}
		}
		match super::diskann::DiskIndex::open(&dir) {
			Ok(idx) => (Some(idx), false),
			Err(_) => {
				// Missing or corrupt — a full build is the only option.
				let built = self.build_disk_snapshot(subdir, items.to_vec());
				(built, true)
			}
		}
	}

	// COST: the Vamana build runs under the graph WRITE lock.
	pub fn consolidate_disk_index(&mut self) {
		if !matches!(self.entity_idx, VectorBackend::Disk { .. }) {
			return;
		}
		let epoch = self.store().as_ref().map(|s| s.read_epoch());
		let fresh = |subdir: &str| {
			let dir = Path::new(&self.data_dir).join("diskann").join(subdir);
			// No store means no epoch to compare against — treat as stale so a
			// store-less graph (tests) still folds its delta like the old path did.
			match epoch {
				Some(e) => super::diskann::snapshot_epoch(&dir) == Some(e),
				None => false,
			}
		};
		let (e_fresh, g_fresh, r_fresh) = (fresh("entity"), fresh("gnn"), fresh("reason"));
		if e_fresh && g_fresh && r_fresh {
			return;
		}
		// Rebuild only the stale indexes. A fresh one keeps its delta overlay —
		// replacing it from the snapshot would silently drop in-process entities.
		if !e_fresh {
			self.entity_idx = VectorBackend::resident(16, 200, self.quant_mode);
			match self.build_disk_snapshot("entity", self.collect_entity_items()) {
				Some(snapshot) => self.entity_idx = VectorBackend::disk(snapshot, self.quant_mode),
				None => {
					self.rebuild_index();
					return;
				}
			}
		}
		if !g_fresh {
			self.gnn_entity_idx = VectorBackend::resident(16, 200, self.quant_mode);
			match self.build_disk_snapshot("gnn", self.collect_gnn_items()) {
				Some(snapshot) => self.gnn_entity_idx = VectorBackend::disk(snapshot, self.quant_mode),
				None => {
					self.rebuild_index();
					return;
				}
			}
		}
		if !r_fresh {
			self.reason_idx = VectorBackend::resident(16, 200, self.quant_mode);
			match self.build_disk_snapshot("reason", self.collect_reason_items()) {
				Some(snapshot) => self.reason_idx = VectorBackend::disk(snapshot, self.quant_mode),
				None => self.rebuild_index(),
			}
		}
	}

	/// Take the Rephrase edges re-pointed at a supersede, for the tick loop to
	/// re-enqueue as `ClassifyContradiction` (ROADMAP item 60).
	pub fn drain_pending_reclass(&self) -> Vec<(String, String)> {
		std::mem::take(&mut *self.pending_reclass.lock())
	}

	pub fn push_reclass(&self, kern_id: &str, reason_id: &str) {
		self
			.pending_reclass
			.lock()
			.push((kern_id.to_string(), reason_id.to_string()));
	}

	pub fn pending_disk_delta_len(&self) -> usize {
		self.entity_idx.pending_delta_len()
	}

	pub fn get(&mut self, id: &str) -> Option<&Kern> {
		if self.kerns.contains_key(id) {
			if let Some(k) = self.kerns.get_mut(id) {
				k.last_access = Some(SystemTime::now());
			}
			return self.kerns.get(id);
		}
		if self.unloaded.contains(id) {
			let loaded = self
				.store
				.clone()
				.and_then(|s| s.load_one_kern(id).ok().flatten());
			if let Some(mut k) = loaded {
				k.last_access = Some(SystemTime::now());
				index_kern_into(
					&k,
					&mut self.entity_kern,
					&mut self.reason_kern,
					&mut self.src_index,
					Some(&mut self.entity_idx),
					Some(&mut self.gnn_entity_idx),
					Some(&mut self.reason_idx),
				);
				self.unloaded.remove(id);
				self.kerns.insert(id.to_string(), k);
				return self.kerns.get(id);
			}
		}
		None
	}

	// Direct map access, same contract as `kerns.get_mut(&root.id)` — no load,
	// no epoch bump. Use `get_mut` when either matters.
	pub fn root_kern_mut(&mut self) -> Option<&mut Kern> {
		let id = self.root.id.clone();
		self.kerns.get_mut(&id)
	}

	pub fn get_mut(&mut self, id: &str) -> Option<&mut Kern> {
		if !self.kerns.contains_key(id) {
			self.get(id);
		}
		if self.kerns.contains_key(id) {
			self.bump_mutation_epoch();
		}
		if let Some(k) = self.kerns.get_mut(id) {
			k.last_access = Some(SystemTime::now());
			Some(k)
		} else {
			None
		}
	}

	// INVARIANT: every content mutation must move this epoch — via `get_mut`,
	// or an explicit bump on the paths that reach `kerns` directly
	// (`remove_entity`, `move_entity`, `merge_remote_entity`,
	// `degrade_entity_reasons`). The daemon's query cache is keyed on it; a
	// mutation the epoch misses is a stale result served after a delete.
	pub fn bump_mutation_epoch(&mut self) {
		self.mutation_epoch = self.mutation_epoch.wrapping_add(1);
	}

	pub fn bump_lamport(&self) -> u64 {
		self
			.lamport
			.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
			+ 1
	}

	pub fn observe_lamport(&self, remote: u64) {
		let mut current = self.lamport.load(std::sync::atomic::Ordering::SeqCst);
		while remote > current {
			match self.lamport.compare_exchange(
				current,
				remote + 1,
				std::sync::atomic::Ordering::SeqCst,
				std::sync::atomic::Ordering::SeqCst,
			) {
				Ok(_) => break,
				Err(actual) => current = actual,
			}
		}
	}

	pub fn mutation_epoch(&self) -> u64 {
		self.mutation_epoch
	}

	pub fn entity_adjacency(&self) -> Arc<EntityAdjacency> {
		let epoch = self.mutation_epoch;
		{
			let cached = self.adjacency_cache.read();
			if let Some((e, adj)) = cached.as_ref() {
				if *e == epoch {
					return adj.clone();
				}
			}
		}
		let adj = Arc::new(EntityAdjacency::build(self));
		*self.adjacency_cache.write() = Some((epoch, adj.clone()));
		adj
	}

	pub fn register(&mut self, kern: Kern) {
		let kid = kern.id.clone();
		for t in kern.entities.values() {
			self.entity_kern.insert(t.id.clone(), kid.clone());
		}
		for r in kern.reasons.values() {
			self.reason_kern.insert(r.id.clone(), kid.clone());
		}
		self.unloaded.remove(&kid);
		self.bump_mutation_epoch();
		self.kerns.insert(kid, kern);
		self.enforce_kern_cap();
	}

	pub fn index_entity(&mut self, entity_id: &str, kern_id: &str) {
		self
			.entity_kern
			.insert(entity_id.to_string(), kern_id.to_string());
	}

	pub fn unindex_entity(&mut self, entity_id: &str) {
		self.entity_kern.remove(entity_id);
	}

	pub fn index_reason(&mut self, reason_id: &str, kern_id: &str) {
		self
			.reason_kern
			.insert(reason_id.to_string(), kern_id.to_string());
	}

	pub fn unindex_reason(&mut self, reason_id: &str) {
		self.reason_kern.remove(reason_id);
	}

	pub fn kern_of_entity(&self, entity_id: &str) -> Option<&str> {
		self.entity_kern.get(entity_id).map(|s| s.as_str())
	}

	pub fn kern_of_reason(&self, reason_id: &str) -> Option<&str> {
		self.reason_kern.get(reason_id).map(|s| s.as_str())
	}

	pub fn kern_of_source(&self, external_id: &str) -> Option<&str> {
		self.src_index.get(external_id).map(|s| s.as_str())
	}

	pub fn set_source_entry(&mut self, external_id: String, kern_id: String) {
		self.src_index.insert(external_id, kern_id);
	}

	/// Drop a source-keyed entry — for a renamed file whose old path no longer
	/// exists. `set_source_entry` reassigns; this clears.
	pub fn clear_source_entry(&mut self, external_id: &str) {
		self.src_index.remove(external_id);
	}

	pub fn loaded(&self, id: &str) -> Option<&Kern> {
		self.kerns.get(id)
	}

	/// Resident-map misses are ambiguous: a kern can be unloaded (on disk,
	/// reloadable) or genuinely gone. Anything that deletes on a miss must
	/// check this first — deregister on an unloaded kern erases its disk row.
	pub fn is_unloaded(&self, id: &str) -> bool {
		self.unloaded.contains(id)
	}

	pub fn count(&self) -> usize {
		self.kerns.len() + self.unloaded.len()
	}

	pub fn deregister(&mut self, id: &str) {
		if let Some(kern) = self.kerns.get(id) {
			for tid in kern.entities.keys() {
				self.entity_kern.remove(tid);
			}
			for rid in kern.reasons.keys() {
				self.reason_kern.remove(rid);
			}
		}
		self.kerns.remove(id);
		self.unloaded.remove(id);
		self.bump_mutation_epoch();
		// Delete the on-disk row so a deregistered kern does not resurrect on load.
		if let Some(store) = &self.store {
			let _ = store.delete_one_kern(id);
		}
	}

	pub fn unload(&mut self, id: &str) -> Result<(), StoreError> {
		if id == self.root.id || !self.kerns.contains_key(id) {
			return Ok(());
		}
		// Unloading is residency, never forgetting: `get` reloads through the
		// store, so without one the kern would leave RAM with nothing to come
		// back from.
		let Some(store) = self.store.clone() else {
			return Ok(());
		};
		if let Some(k) = self.kerns.get(id) {
			store.save_one_kern(k)?;
		}
		self.kerns.remove(id);
		self.unloaded.insert(id.to_string());
		Ok(())
	}

	fn gc_empty_kerns(&mut self) -> usize {
		let root_id = self.root.id.clone();

		// Cycle-safe via the `live` visited-set: re-encountering a live id stops.
		let mut live: std::collections::HashSet<String> = std::collections::HashSet::new();
		for k in self.kerns.values() {
			if k.id != root_id && !k.is_named() && k.entities.is_empty() {
				continue;
			}
			let mut cur = k.id.clone();
			loop {
				if !live.insert(cur.clone()) {
					break;
				}
				let parent = match self.kerns.get(&cur) {
					Some(pk) => pk.parent.clone(),
					None => break,
				};
				if parent.is_empty() || parent == cur {
					break;
				}
				cur = parent;
			}
		}
		live.insert(root_id.clone());

		let victims: std::collections::HashSet<String> = self
			.kerns
			.keys()
			.filter(|id| !live.contains(*id))
			.cloned()
			.collect();
		if victims.is_empty() {
			return 0;
		}

		let removed = victims.len();
		for id in &victims {
			self.deregister(id);
		}

		let existing: std::collections::HashSet<String> = self.kerns.keys().cloned().collect();
		for k in self.kerns.values_mut() {
			if !k.children.is_empty() {
				k.children.retain(|c| existing.contains(c));
			}
		}
		removed
	}

	pub fn gc_empty_kerns_counted(&mut self) -> (usize, usize, usize) {
		let before = self.kerns.len();
		let reaped = self.gc_empty_kerns();
		(before, reaped, self.kerns.len())
	}

	pub fn all(&self) -> Vec<&Kern> {
		self.kerns.values().collect()
	}

	pub fn all_ids(&self) -> Vec<String> {
		let mut ids: Vec<String> = self.kerns.keys().cloned().collect();
		ids.extend(self.unloaded.iter().cloned());
		ids
	}

	pub fn map(&self) -> &HashMap<String, Kern> {
		&self.kerns
	}

	/// Kerns unloaded from RAM whose rows still live on disk. The flush prune
	/// must spare these ids — deleting them turns residency into forgetting.
	pub fn unloaded_ids(&self) -> &HashSet<String> {
		&self.unloaded
	}

	pub fn from_saved_with_mode(
		root: Kern,
		replica_id: String,
		data_dir: String,
		kerns: HashMap<String, Kern>,
		unloaded: HashSet<String>,
		quant_mode: QuantizationMode,
	) -> Self {
		let mut g = Self {
			root: root.clone(),
			replica_id,
			data_dir,
			lamport: std::sync::atomic::AtomicU64::new(0),
			pending_reclass: parking_lot::Mutex::new(Vec::new()),
			store: None,
			quant_mode,
			entity_idx: VectorBackend::resident(16, 200, quant_mode),
			gnn_entity_idx: VectorBackend::resident(16, 200, quant_mode),
			reason_idx: VectorBackend::resident(16, 200, quant_mode),
			kerns,
			unloaded,
			src_index: HashMap::new(),
			entity_kern: HashMap::new(),
			reason_kern: HashMap::new(),
			lexical: Some(Arc::new(LexicalIndex::new_in_ram(1.2, 0.75))),
			max_loaded_kerns: KERN_CAP_DISABLED,
			// Spill by default (RECALL_PLAN F4): the first rebuild_index after load
			// must take the mmap'd DiskANN path, not rebuild the resident HNSW
			// indexes (~4.5s on a real store). apply_graph_config re-applies the
			// configured threshold; an explicit KERN_CAP_DISABLED opt-out rebuilds
			// resident exactly once there.
			disk_threshold: 0,
			mutation_epoch: 0,
			flushed_epoch: 0,
			adjacency_cache: parking_lot::RwLock::new(None),
			entity_dim_cache: parking_lot::RwLock::new(None),
			embed_model: String::new(),
		};
		g.rebuild_index();
		if let Some(lex) = g.lexical.clone() {
			lex.rebuild_from_graph(&g);
		}
		g
	}
}

#[cfg(test)]
#[path = "tests/graph_test.rs"]
mod graph_tests;
