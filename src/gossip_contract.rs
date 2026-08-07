//! Hosted-contract enforcement: a contract names its owners, writer allowlist,
//! entity kinds, caps and retention; deltas that violate it are refused and
//! counted. Params are policy — amending them mints a new contract id, with an
//! owner-signed tombstone pointing subscribers at the successor.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use base::base_types::{Entity, EntityKind};
use graph::graph::GraphGnn;

use crate::gossip_identity::{loc_of, verify_sig_by};

/// The key IS the policy: a shared kern is addressed by the hash of its
/// validation policy + parameters, so any peer holding the key knows exactly
/// what writes are admissible and no authority is needed (FEDERATION_PLAN §3).
pub type ContractId = [u8; 32];

pub const SIGNED_CRDT_V0_TAG: &str = "signed-crdt-v0";

// Deltas refused by a contract's validation. Refusals are counted, never
// panicked on — a hostile delta is expected weather.
static CONTRACT_REFUSED: AtomicU64 = AtomicU64::new(0);

pub fn contract_refused() -> u64 {
	CONTRACT_REFUSED.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WritePolicy {
	Open,
	Allowlist(Vec<[u8; 32]>),
	OwnersOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivacyV0 {
	/// 0 = xchacha20poly1305 (the only scheme in v0).
	pub scheme: u8,
	pub key_hint: [u8; 8],
}

/// Contract parameters, v0. Canonical bincode of this struct is hashed into
/// the ContractId, so ANY change to a field mints a different key — policy
/// changes move the key by construction (see Tombstone in §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamsV0 {
	/// May sign anything, including params amendments.
	pub owners: Vec<[u8; 32]>,
	pub writers: WritePolicy,
	/// Admissible claim kinds; None = all.
	pub kinds: Option<Vec<EntityKind>>,
	/// Hard cap; replaces the global remote cap for this kern.
	pub max_entities: u32,
	/// Forced TTL stamped on every entity at apply time.
	pub retention_secs: Option<u64>,
	pub private: Option<PrivacyV0>,
}

impl ParamsV0 {
	fn writer_admissible(&self, signer: &[u8; 32]) -> bool {
		if self.owners.contains(signer) {
			return true;
		}
		match &self.writers {
			WritePolicy::Open => true,
			WritePolicy::Allowlist(keys) => keys.contains(signer),
			WritePolicy::OwnersOnly => false,
		}
	}

	fn kind_admissible(&self, kind: EntityKind) -> bool {
		match &self.kinds {
			None => true,
			Some(list) => list.contains(&kind),
		}
	}
}

pub fn contract_id(kind_tag: &str, params: &ParamsV0) -> ContractId {
	let bytes = bincode::serde::encode_to_vec(params, bincode::config::standard())
		.expect("ParamsV0 always encodes");
	let mut h = blake3::Hasher::new();
	h.update(kind_tag.as_bytes());
	h.update(&[0u8]);
	h.update(&bytes);
	*h.finalize().as_bytes()
}

/// Ring location of the shared kern — where its subscription tree roots.
pub fn contract_loc(id: &ContractId) -> f64 {
	loc_of(id)
}

/// The graph kern a contract's entities merge into. The `remote-` prefix
/// keeps every existing trust boundary (`is_remote_kern_id`) intact.
pub fn contract_kern_id(id: &ContractId) -> String {
	format!("remote-contract-{}", util::hex::encode(id))
}

/// What a writer signs: blake3 over a domain tag, the entity id and the
/// writer's lamport. The id already binds the body (content addressing), so
/// signing the id transitively signs the text.
pub fn entity_sig_digest(entity_id: &str, lamport: u64) -> [u8; 32] {
	let mut h = blake3::Hasher::new();
	h.update(b"kern-contract-entity");
	h.update(&[0u8]);
	h.update(entity_id.as_bytes());
	h.update(&lamport.to_le_bytes());
	*h.finalize().as_bytes()
}

/// Decode a hex-encoded 32-byte key (ed25519 pubkey or contract id).
pub fn parse_key_hex(s: &str) -> Option<[u8; 32]> {
	util::hex::decode(s.trim())
		.filter(|v| v.len() == 32)
		.and_then(|v| v.try_into().ok())
}

/// Materialize a `[[gossip.contracts]]` table into params. None when a key
/// fails to parse — silently weakening a write policy is worse than refusing
/// to host the contract.
pub fn params_from_config(c: &config::ContractConfig) -> Option<ParamsV0> {
	if c.kind != SIGNED_CRDT_V0_TAG {
		return None;
	}
	let mut owners = Vec::new();
	for o in &c.owners {
		owners.push(parse_key_hex(o)?);
	}
	let writers = match c.writers.as_str() {
		"open" => WritePolicy::Open,
		"owners-only" => WritePolicy::OwnersOnly,
		"allowlist" => {
			let mut keys = Vec::new();
			for k in &c.writer_keys {
				keys.push(parse_key_hex(k)?);
			}
			WritePolicy::Allowlist(keys)
		}
		_ => return None,
	};
	let kinds = if c.kinds.is_empty() {
		None
	} else {
		let mut list = Vec::new();
		for k in &c.kinds {
			list.push(EntityKind::parse(k)?);
		}
		Some(list)
	};
	Some(ParamsV0 {
		owners,
		writers,
		kinds,
		max_entities: c.max_entities,
		retention_secs: c.retention_secs,
		private: None,
	})
}

/// What an owner signs to retire a contract in favour of an amended one:
/// the old key, the new key, a domain tag. Amending params moves the key
/// (key = policy hash), so the tombstone is the forward pointer subscribers
/// follow once (FEDERATION_PLAN §5).
pub fn tombstone_digest(old: &ContractId, new_id: &ContractId) -> [u8; 32] {
	let mut h = blake3::Hasher::new();
	h.update(b"kern-contract-tombstone");
	h.update(&[0u8]);
	h.update(old);
	h.update(new_id);
	*h.finalize().as_bytes()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedEntity {
	pub entity: Entity,
	pub signer: [u8; 32],
	pub lamport: u64,
	pub sig: Vec<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Delta {
	pub entities: Vec<SignedEntity>,
}

/// Compact digest of a contract state: sorted (id, lamport) pairs bucketed on
/// the first hex nibble of the id, each bucket hashed, the root over the
/// bucket hashes. `entries` rides along so `diff` can name exactly the
/// missing/stale ids without another round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
	pub root: [u8; 32],
	pub buckets: Vec<[u8; 32]>,
	pub entries: Vec<(String, u64)>,
}

/// The signed bodies a node can prove: what `summarize`/`diff` speak over.
/// Kept beside the graph (not in it) because the graph stores entities, and
/// only the envelope here can re-serve a body with its writer's signature.
#[derive(Default)]
pub struct ContractState {
	pub entries: HashMap<String, SignedEntity>,
}

/// First hex digit of the first byte of a content-hash id (0-15).
/// Buckets sync summaries by this nibble so peers skip matching buckets.
fn hex_nibble(id: &str) -> usize {
	id.bytes()
		.next()
		.map(|b| match b {
			b'0'..=b'9' => (b - b'0') as usize,
			b'a'..=b'f' => (b - b'a' + 10) as usize,
			_ => 0,
		})
		.unwrap_or(0)
}

impl ContractState {
	pub fn len(&self) -> usize {
		self.entries.len()
	}

	pub fn is_empty(&self) -> bool {
		self.entries.is_empty()
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
	pub changed: bool,
	pub merged: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
	ForgedId,
	BadSignature,
	WriterNotAdmissible,
	KindNotAdmissible,
	OverCap,
}

/// The contract seam. v0 ships exactly one implementation; wasm contracts
/// (key = hash of the module) reuse this seam later behind the `plugins`
/// feature — NOT in v0.
pub trait SyncContract: Send + Sync {
	fn validate_delta(
		&self,
		params: &ParamsV0,
		state: &ContractState,
		delta: &Delta,
	) -> Result<(), Refusal>;
	fn summarize(&self, state: &ContractState) -> Summary;
	fn diff(&self, state: &ContractState, remote: &Summary) -> Delta;
	/// Must be commutative and idempotent — it is `merge.rs` behind a trait.
	fn apply(
		&self,
		g: &mut GraphGnn,
		kern_id: &str,
		params: &ParamsV0,
		state: &mut ContractState,
		delta: Delta,
	) -> Applied;
}

/// Builtin contract v0: every entity body must hash to its id, carry an
/// admissible writer's signature, match the kind filter and fit the cap.
/// Apply is the existing CRDT merge (`merge_remote_entity`), nothing new.
pub struct SignedCrdt;

impl SignedCrdt {
	fn refuse(r: Refusal) -> Result<(), Refusal> {
		CONTRACT_REFUSED.fetch_add(1, Ordering::Relaxed);
		Err(r)
	}
}

impl SyncContract for SignedCrdt {
	fn validate_delta(
		&self,
		params: &ParamsV0,
		state: &ContractState,
		delta: &Delta,
	) -> Result<(), Refusal> {
		let mut new_ids = 0usize;
		for se in &delta.entities {
			if !crate::gossip_handler::id_matches_body(&se.entity) {
				return Self::refuse(Refusal::ForgedId);
			}
			let digest = entity_sig_digest(&se.entity.id, se.lamport);
			if !verify_sig_by(&se.signer, &digest, &se.sig) {
				return Self::refuse(Refusal::BadSignature);
			}
			if !params.writer_admissible(&se.signer) {
				return Self::refuse(Refusal::WriterNotAdmissible);
			}
			if !params.kind_admissible(se.entity.kind) {
				return Self::refuse(Refusal::KindNotAdmissible);
			}
			if !state.entries.contains_key(&se.entity.id) {
				new_ids += 1;
			}
		}
		if state.entries.len() + new_ids > params.max_entities as usize {
			return Self::refuse(Refusal::OverCap);
		}
		Ok(())
	}

	fn summarize(&self, state: &ContractState) -> Summary {
		let mut entries: Vec<(String, u64)> = state
			.entries
			.values()
			.map(|se| (se.entity.id.clone(), se.lamport))
			.collect();
		entries.sort();
		let mut buckets = vec![blake3::Hasher::new(); 16];
		for (id, lamport) in &entries {
			let nibble = hex_nibble(id);
			buckets[nibble].update(id.as_bytes());
			buckets[nibble].update(&lamport.to_le_bytes());
		}
		let buckets: Vec<[u8; 32]> = buckets
			.into_iter()
			.map(|h| *h.finalize().as_bytes())
			.collect();
		let mut root = blake3::Hasher::new();
		for b in &buckets {
			root.update(b);
		}
		Summary {
			root: *root.finalize().as_bytes(),
			buckets,
			entries,
		}
	}

	fn diff(&self, state: &ContractState, remote: &Summary) -> Delta {
		let local = self.summarize(state);
		if local.root == remote.root {
			return Delta::default();
		}
		let remote_lamports: HashMap<&str, u64> = remote
			.entries
			.iter()
			.map(|(id, l)| (id.as_str(), *l))
			.collect();
		let mut out = Vec::new();
		for (id, lamport) in &local.entries {
			// Matching bucket hash = every (id, lamport) in it agrees; skip.
			let nibble = hex_nibble(id);
			if remote.buckets.get(nibble) == local.buckets.get(nibble) {
				continue;
			}
			let missing_or_stale = match remote_lamports.get(id.as_str()) {
				None => true,
				Some(theirs) => *theirs < *lamport,
			};
			if missing_or_stale {
				if let Some(se) = state.entries.get(id) {
					out.push(se.clone());
				}
			}
		}
		Delta { entities: out }
	}

	fn apply(
		&self,
		g: &mut GraphGnn,
		kern_id: &str,
		params: &ParamsV0,
		state: &mut ContractState,
		delta: Delta,
	) -> Applied {
		let mut changed = false;
		let mut merged = 0usize;
		for se in delta.entities {
			let mut entity = se.entity.clone();
			// The contract's retention is a forced TTL: stamp it unless the
			// entity already expires sooner.
			if let Some(secs) = params.retention_secs {
				let deadline = std::time::SystemTime::now() + std::time::Duration::from_secs(secs);
				entity.valid_until = Some(match entity.valid_until {
					Some(existing) if existing < deadline => existing,
					_ => deadline,
				});
			}
			let entry_changed = match state.entries.get(&se.entity.id) {
				// Idempotence at the envelope level: an equal-or-older
				// lamport is a redelivery, not news.
				Some(existing) if existing.lamport >= se.lamport => false,
				_ => {
					state.entries.insert(se.entity.id.clone(), se.clone());
					true
				}
			};
			let graph_changed = graph::merge::merge_remote_entity(g, kern_id, entity);
			if graph_changed {
				merged += 1;
			}
			changed |= entry_changed || graph_changed;
		}
		Applied { changed, merged }
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::gossip_identity::PeerIdentity;
	use base::base_types::{ChunkPart, ChunkPartKind, Kern};

	fn entity_of(text: &str, kind: EntityKind) -> Entity {
		Entity {
			id: util::content_hash(text),
			kind,
			statements: vec![text.to_string()],
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		}
	}

	fn signed(identity: &PeerIdentity, text: &str, kind: EntityKind, lamport: u64) -> SignedEntity {
		let entity = entity_of(text, kind);
		let digest = entity_sig_digest(&entity.id, lamport);
		SignedEntity {
			sig: identity.sign_digest(&digest),
			signer: identity.pubkey(),
			lamport,
			entity,
		}
	}

	fn open_params() -> ParamsV0 {
		ParamsV0 {
			owners: Vec::new(),
			writers: WritePolicy::Open,
			kinds: None,
			max_entities: 100,
			retention_secs: None,
			private: None,
		}
	}

	fn graph_with_contract_kern(id: &ContractId) -> GraphGnn {
		let mut g = GraphGnn::new();
		let kid = contract_kern_id(id);
		let mut k = Kern::new(&kid, &g.root.id);
		k.root_id = g.root.root_id.clone();
		g.register(k);
		g
	}

	#[test]
	fn the_key_is_the_policy_any_param_change_moves_the_contract_id() {
		let base = open_params();
		let id = contract_id(SIGNED_CRDT_V0_TAG, &base);
		assert_eq!(
			id,
			contract_id(SIGNED_CRDT_V0_TAG, &base),
			"same policy, same key — deterministic"
		);

		let owner = PeerIdentity::from_bytes([1u8; 32]).pubkey();
		let mut with_owner = open_params();
		with_owner.owners.push(owner);
		assert_ne!(id, contract_id(SIGNED_CRDT_V0_TAG, &with_owner));

		let mut tighter = open_params();
		tighter.writers = WritePolicy::OwnersOnly;
		assert_ne!(id, contract_id(SIGNED_CRDT_V0_TAG, &tighter));

		assert_ne!(
			id,
			contract_id("other-kind", &base),
			"the kind tag participates in the key"
		);
	}

	#[test]
	fn validate_refuses_the_forged_the_unsigned_the_foreign_and_the_off_kind() {
		let c = SignedCrdt;
		let state = ContractState::default();
		let writer = PeerIdentity::from_bytes([2u8; 32]);
		let stranger = PeerIdentity::from_bytes([3u8; 32]);

		let mut owners_only = open_params();
		owners_only.owners.push(writer.pubkey());
		owners_only.writers = WritePolicy::OwnersOnly;

		let before = contract_refused();

		// Forged id: body does not hash to the claimed id.
		let mut forged = signed(&writer, "honest text", EntityKind::Fact, 1);
		forged.entity.statements = vec!["substituted".into()];
		assert_eq!(
			c.validate_delta(
				&owners_only,
				&state,
				&Delta {
					entities: vec![forged]
				}
			),
			Err(Refusal::ForgedId)
		);

		// Bad signature: right writer, wrong digest.
		let mut unsigned = signed(&writer, "honest text", EntityKind::Fact, 1);
		unsigned.sig = writer
			.sign_digest(&entity_sig_digest("other-id", 1))
			.clone();
		assert_eq!(
			c.validate_delta(
				&owners_only,
				&state,
				&Delta {
					entities: vec![unsigned]
				}
			),
			Err(Refusal::BadSignature)
		);

		// Inadmissible writer: valid signature by a non-owner under OwnersOnly.
		let foreign = signed(&stranger, "honest text", EntityKind::Fact, 1);
		assert_eq!(
			c.validate_delta(
				&owners_only,
				&state,
				&Delta {
					entities: vec![foreign]
				}
			),
			Err(Refusal::WriterNotAdmissible)
		);

		// Off-kind: contract admits only Facts.
		let mut facts_only = open_params();
		facts_only.kinds = Some(vec![EntityKind::Fact]);
		let claim = signed(&writer, "a claim", EntityKind::Claim, 1);
		assert_eq!(
			c.validate_delta(
				&facts_only,
				&state,
				&Delta {
					entities: vec![claim]
				}
			),
			Err(Refusal::KindNotAdmissible)
		);

		assert!(
			contract_refused() >= before + 4,
			"every refusal is counted, never panicked on"
		);

		// The honest delta passes the same gauntlet.
		let honest = signed(&writer, "honest text", EntityKind::Fact, 1);
		assert_eq!(
			c.validate_delta(
				&owners_only,
				&state,
				&Delta {
					entities: vec![honest]
				}
			),
			Ok(())
		);
	}

	#[test]
	fn validate_refuses_a_delta_that_would_breach_max_entities() {
		let c = SignedCrdt;
		let writer = PeerIdentity::from_bytes([4u8; 32]);
		let mut params = open_params();
		params.max_entities = 2;

		let mut state = ContractState::default();
		let a = signed(&writer, "fact a", EntityKind::Fact, 1);
		state.entries.insert(a.entity.id.clone(), a.clone());
		let b = signed(&writer, "fact b", EntityKind::Fact, 1);
		state.entries.insert(b.entity.id.clone(), b.clone());

		let fresh = signed(&writer, "fact c", EntityKind::Fact, 1);
		assert_eq!(
			c.validate_delta(
				&params,
				&state,
				&Delta {
					entities: vec![fresh]
				}
			),
			Err(Refusal::OverCap)
		);

		// A known id at the cap is a merge, not growth — still admissible.
		assert_eq!(
			c.validate_delta(&params, &state, &Delta { entities: vec![a] }),
			Ok(())
		);
	}

	#[test]
	fn apply_is_commutative_and_idempotent() {
		let c = SignedCrdt;
		let writer = PeerIdentity::from_bytes([5u8; 32]);
		let params = open_params();
		let cid = contract_id(SIGNED_CRDT_V0_TAG, &params);
		let kid = contract_kern_id(&cid);

		let d1 = Delta {
			entities: vec![
				signed(&writer, "alpha", EntityKind::Fact, 1),
				signed(&writer, "beta", EntityKind::Fact, 2),
			],
		};
		let d2 = Delta {
			entities: vec![
				signed(&writer, "beta", EntityKind::Fact, 2),
				signed(&writer, "gamma", EntityKind::Fact, 3),
			],
		};

		let mut g_ab = graph_with_contract_kern(&cid);
		let mut s_ab = ContractState::default();
		c.apply(&mut g_ab, &kid, &params, &mut s_ab, d1.clone());
		c.apply(&mut g_ab, &kid, &params, &mut s_ab, d2.clone());

		let mut g_ba = graph_with_contract_kern(&cid);
		let mut s_ba = ContractState::default();
		c.apply(&mut g_ba, &kid, &params, &mut s_ba, d2.clone());
		c.apply(&mut g_ba, &kid, &params, &mut s_ba, d1.clone());

		assert_eq!(
			c.summarize(&s_ab),
			c.summarize(&s_ba),
			"apply order must not matter"
		);
		let ids = |g: &GraphGnn| {
			let mut v: Vec<String> = g.kerns[&kid].entities.keys().cloned().collect();
			v.sort();
			v
		};
		assert_eq!(ids(&g_ab), ids(&g_ba), "the graphs converge too");

		let again = c.apply(&mut g_ab, &kid, &params, &mut s_ab, d2);
		assert!(!again.changed, "a redelivery is a no-op, not a change");
	}

	#[test]
	fn two_divergent_states_converge_after_one_diff_exchange_each_way() {
		let c = SignedCrdt;
		let writer = PeerIdentity::from_bytes([6u8; 32]);
		let params = open_params();
		let cid = contract_id(SIGNED_CRDT_V0_TAG, &params);
		let kid = contract_kern_id(&cid);

		let mut g_a = graph_with_contract_kern(&cid);
		let mut s_a = ContractState::default();
		let mut g_b = graph_with_contract_kern(&cid);
		let mut s_b = ContractState::default();

		// Shared history plus one private entity each side.
		let shared = Delta {
			entities: vec![signed(&writer, "both know this", EntityKind::Fact, 1)],
		};
		c.apply(&mut g_a, &kid, &params, &mut s_a, shared.clone());
		c.apply(&mut g_b, &kid, &params, &mut s_b, shared);
		c.apply(
			&mut g_a,
			&kid,
			&params,
			&mut s_a,
			Delta {
				entities: vec![signed(&writer, "only a knows", EntityKind::Fact, 2)],
			},
		);
		c.apply(
			&mut g_b,
			&kid,
			&params,
			&mut s_b,
			Delta {
				entities: vec![signed(&writer, "only b knows", EntityKind::Fact, 3)],
			},
		);
		assert_ne!(c.summarize(&s_a).root, c.summarize(&s_b).root);

		// One summary/diff exchange in each direction.
		let a_to_b = c.diff(&s_a, &c.summarize(&s_b));
		assert_eq!(
			a_to_b.entities.len(),
			1,
			"diff ships only the missing id, not the whole state"
		);
		c.apply(&mut g_b, &kid, &params, &mut s_b, a_to_b);
		let b_to_a = c.diff(&s_b, &c.summarize(&s_a));
		c.apply(&mut g_a, &kid, &params, &mut s_a, b_to_a);

		assert_eq!(
			c.summarize(&s_a),
			c.summarize(&s_b),
			"byte-identical summaries after one exchange each direction"
		);
		assert_eq!(
			c.diff(&s_a, &c.summarize(&s_b)).entities.len(),
			0,
			"converged states have an empty diff"
		);
	}

	#[test]
	fn a_contract_retention_stamps_a_ttl_on_every_applied_entity() {
		let c = SignedCrdt;
		let writer = PeerIdentity::from_bytes([7u8; 32]);
		let mut params = open_params();
		params.retention_secs = Some(3_600);
		let cid = contract_id(SIGNED_CRDT_V0_TAG, &params);
		let kid = contract_kern_id(&cid);

		let mut g = graph_with_contract_kern(&cid);
		let mut s = ContractState::default();
		let se = signed(&writer, "expires", EntityKind::Fact, 1);
		let id = se.entity.id.clone();
		c.apply(&mut g, &kid, &params, &mut s, Delta { entities: vec![se] });

		let stored = &g.kerns[&kid].entities[&id];
		let deadline = stored.valid_until.expect("retention forced a TTL");
		let secs = deadline
			.duration_since(std::time::SystemTime::now())
			.expect("deadline is in the future")
			.as_secs();
		assert!(
			(3_500..=3_600).contains(&secs),
			"the deadline is retention_secs out, got {secs}"
		);
	}
}
