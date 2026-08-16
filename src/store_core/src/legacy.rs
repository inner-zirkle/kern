//! Frozen decoders for stores written by earlier builds.
//!
//! The alpha policy is that a persisted format is wiped, never migrated
//! (`AGENTS.md`). This module is the one deliberate exception, and it earns it
//! by being *frozen*: every type here is a snapshot of a layout that shipped,
//! it derives `Deserialize` only, and nothing in kern may ever write one. A
//! current type is reused for any field whose layout did not change — the
//! snapshot names only the delta, so there is less to keep in sync and less to
//! get wrong.
//!
//! ## Why the version byte is not enough
//!
//! `FORMAT_VERSION` is supposed to identify a layout, and for a while it did
//! not: `Entity.trust_tier` was added (f60fbce, 2026-08-15 11:00) with no bump,
//! so **two incompatible `Entity` layouts both call themselves version 10**.
//! That is why `decode_kern_row` tries the candidates for a version rather than
//! trusting it, and why `layout_guard.rs` now fails the build when a persisted
//! struct changes without a bump. A migration is only as honest as the version
//! it keys on.
//!
//! ## Adding the next one
//!
//! When `FORMAT_VERSION` bumps to 12: snapshot the v11 layout here the way v10
//! is snapshotted, add its byte to `READABLE_VERSIONS`, give `decode_kern_row`
//! and `decode_cold` an arm for it, and pin it with a test that decodes bytes
//! laid out the old way (see `tests/legacy_test.rs` — the mirrors there are how
//! you write bytes in a layout this build can no longer produce).

use std::collections::HashMap;

use serde::Deserialize;

use base::base_types::{
	ChunkPart, Embedding, Entity, EntityKind, EntityRef, EntityStatus, Kern, Reason, ReasonKind,
	ReviewState, Source,
};
use base::crdt::GCounter;
use std::time::SystemTime;

use crate::{StoredTemporal, StoredVec};

/// Every persisted layout this build can still read, newest first. `decode_kern_row`
/// walks it in order; the first candidate that decodes cleanly wins.
pub(crate) const READABLE_VERSIONS: &[u8] = &[10];

/// `Entity.trust_tier`, as it was before the field was dropped. Decode-only: the
/// value is discarded on the way forward, since the current `Entity` has no
/// channel-trust field to carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[repr(u8)]
enum TrustTier {
	Stated = 0,
	Inferred = 1,
	Tool = 2,
	Imported = 3,
	#[default]
	Unknown = 4,
}

/// `Entity` as written between f60fbce (2026-08-15) and the v11 bump: identical
/// to the current one but for `trust_tier`, which sat between `review` and
/// `statements`. Position is the whole contract — bincode is positional.
#[derive(Debug, Clone, Default, Deserialize)]
struct EntityTrustTier {
	id: String,
	root_id: String,
	external_id: String,
	superseded_by: String,
	kind: EntityKind,
	status: EntityStatus,
	review: ReviewState,
	trust_tier: TrustTier,
	statements: Vec<String>,
	chunks: Vec<ChunkPart>,
	vector: Embedding,
	gnn_vector: Embedding,
	score: f64,
	conf_alpha: f32,
	conf_beta: f32,
	source: Source,
	created_at: Option<SystemTime>,
	access_count: GCounter,
	accessed_at: Option<SystemTime>,
	heat: f32,
	heat_updated_at: Option<SystemTime>,
	updated_at: Option<SystemTime>,
	valid_until: Option<SystemTime>,
	valid_until_lamport: u64,
	valid_until_producer: String,
	producer_id: String,
	unlinked_count: i32,
	dirty: bool,
	user_id: Option<String>,
	agent_id: Option<String>,
	session_id: Option<String>,
	// Same `serde(skip)` as the current type: these live in StoredKern's side
	// map, never in the entity bytes. Skipping them is part of the layout.
	#[serde(skip)]
	valid_from: Option<SystemTime>,
	#[serde(skip)]
	valid_to: Option<SystemTime>,
	#[serde(skip)]
	invalidated_at: Option<SystemTime>,
}

impl From<EntityTrustTier> for Entity {
	fn from(e: EntityTrustTier) -> Self {
		// `trust_tier` is dropped: the current graph scores by source trust, and
		// inventing a field to park it in would be migrating data into nothing.
		let _ = e.trust_tier;
		Entity {
			id: e.id,
			root_id: e.root_id,
			external_id: e.external_id,
			superseded_by: e.superseded_by,
			kind: e.kind,
			status: e.status,
			review: e.review,
			statements: e.statements,
			chunks: e.chunks,
			vector: e.vector,
			gnn_vector: e.gnn_vector,
			score: e.score,
			conf_alpha: e.conf_alpha,
			conf_beta: e.conf_beta,
			source: e.source,
			created_at: e.created_at,
			access_count: e.access_count,
			accessed_at: e.accessed_at,
			heat: e.heat,
			heat_updated_at: e.heat_updated_at,
			updated_at: e.updated_at,
			valid_until: e.valid_until,
			valid_until_lamport: e.valid_until_lamport,
			valid_until_producer: e.valid_until_producer,
			producer_id: e.producer_id,
			unlinked_count: e.unlinked_count,
			dirty: e.dirty,
			user_id: e.user_id,
			agent_id: e.agent_id,
			session_id: e.session_id,
			valid_from: e.valid_from,
			valid_to: e.valid_to,
			invalidated_at: e.invalidated_at,
		}
	}
}

/// `Reason` as written before the v11 bump: `to_net_id` sat between
/// `to_kern_id` and `kind`, carrying the federation peer an edge pointed at.
#[derive(Debug, Clone, Default, Deserialize)]
struct ReasonV10 {
	id: String,
	from: String,
	to: String,
	to_kern_id: String,
	to_net_id: String,
	kind: ReasonKind,
	text: String,
	vector: Embedding,
	score: f64,
	score_lamport: u64,
	score_producer: String,
	traversal_count: GCounter,
	producer_id: String,
	dirty: bool,
}

impl From<ReasonV10> for Reason {
	fn from(r: ReasonV10) -> Self {
		// Federation is gone, so an edge that pointed at a peer points nowhere.
		// It is kept rather than dropped: the endpoints are still local ids, and
		// a silently thinner graph is worse than an edge that no longer reaches.
		let _ = r.to_net_id;
		Reason {
			id: r.id,
			from: r.from,
			to: r.to,
			to_kern_id: r.to_kern_id,
			kind: r.kind,
			text: r.text,
			vector: r.vector,
			score: r.score,
			score_lamport: r.score_lamport,
			score_producer: r.score_producer,
			traversal_count: r.traversal_count,
			producer_id: r.producer_id,
			dirty: r.dirty,
		}
	}
}

/// `Kern` never changed shape across this hop — only what it holds did, which is
/// why the entity type is a parameter. `E` is the entity layout to try.
#[derive(Debug, Clone, Deserialize)]
struct KernV10<E> {
	id: String,
	root_id: String,
	graviton_text: String,
	graviton_vec: Vec<f32>,
	inner_radius: f64,
	outer_radius: f64,
	spawn_reason_id: String,
	parent: String,
	children: Vec<String>,
	entities: HashMap<String, E>,
	refs: HashMap<String, EntityRef>,
	reasons: HashMap<String, ReasonV10>,
	by_from: HashMap<String, Vec<String>>,
	by_to: HashMap<String, Vec<String>>,
	source_index: HashMap<String, String>,
	claim_kinds: HashMap<String, String>,
	claim_kind_parents: HashMap<String, String>,
	gnn_weights: Vec<u8>,
	mass: f64,
	#[serde(skip)]
	last_access: Option<SystemTime>,
}

impl<E: Into<Entity>> From<KernV10<E>> for Kern {
	fn from(k: KernV10<E>) -> Self {
		Kern {
			id: k.id,
			root_id: k.root_id,
			graviton_text: k.graviton_text,
			graviton_vec: k.graviton_vec,
			inner_radius: k.inner_radius,
			outer_radius: k.outer_radius,
			spawn_reason_id: k.spawn_reason_id,
			parent: k.parent,
			children: k.children,
			entities: k.entities.into_iter().map(|(i, e)| (i, e.into())).collect(),
			refs: k.refs,
			reasons: k.reasons.into_iter().map(|(i, r)| (i, r.into())).collect(),
			by_from: k.by_from,
			by_to: k.by_to,
			source_index: k.source_index,
			claim_kinds: k.claim_kinds,
			claim_kind_parents: k.claim_kind_parents,
			gnn_weights: k.gnn_weights,
			mass: k.mass,
			last_access: k.last_access,
		}
	}
}

/// The stored row wrapper. Its own shape is unchanged; only `kern` varies.
#[derive(Deserialize)]
struct StoredKernV10<E> {
	kern: KernV10<E>,
	entity_vecs: HashMap<String, StoredVec>,
	reason_vecs: HashMap<String, StoredVec>,
	temporal: HashMap<String, StoredTemporal>,
}

/// One decoded legacy row, in current types, ready for `StoredKern::into_kern`'s
/// side-map restore. Returned as its parts so this module never has to build a
/// `StoredKern` (which is a *writing* type, and legacy code must not write).
pub(crate) struct LegacyRow {
	pub kern: Kern,
	pub entity_vecs: HashMap<String, StoredVec>,
	pub reason_vecs: HashMap<String, StoredVec>,
	pub temporal: HashMap<String, StoredTemporal>,
}

impl<E: Into<Entity>> From<StoredKernV10<E>> for LegacyRow {
	fn from(s: StoredKernV10<E>) -> Self {
		LegacyRow {
			kern: s.kern.into(),
			entity_vecs: s.entity_vecs,
			reason_vecs: s.reason_vecs,
			temporal: s.temporal,
		}
	}
}

/// Decode one already-decompressed v10 kern row.
///
/// Tries the post-`trust_tier` layout first, then the one before it — the two
/// that both call themselves version 10. Order matters only for speed: a body
/// that decodes as one is overwhelmingly unlikely to decode as the other, since
/// `trust_tier` shifts every field after it and bincode validates enum tags and
/// length prefixes as it goes. `None` means neither fit, which the caller
/// reports as the bad version it is rather than guessing further.
pub(crate) fn decode_v10_kern(raw: &[u8]) -> Option<LegacyRow> {
	let cfg = crate::bincode_cfg();
	// Why each candidate was rejected, logged together on total failure. A
	// migration that just says "no" leaves the operator with an unreadable store
	// and nothing to act on; the bincode error names the field it choked on.
	let mut why: Vec<String> = Vec::new();

	match bincode::serde::decode_from_slice::<StoredKernV10<EntityTrustTier>, _>(raw, cfg) {
		Ok((row, _)) => {
			let row: LegacyRow = row.into();
			if plausible(&row) {
				return Some(row);
			}
			why.push("post-trust_tier: decoded but carried no kern id".to_string());
		}
		Err(e) => why.push(format!("post-trust_tier: {e}")),
	}
	// The pre-f60fbce layout: an entity with no `trust_tier` is byte-identical to
	// the current one, so the current type IS the older snapshot.
	match bincode::serde::decode_from_slice::<StoredKernV10<Entity>, _>(raw, cfg) {
		Ok((row, _)) => {
			let row: LegacyRow = row.into();
			if plausible(&row) {
				return Some(row);
			}
			why.push("pre-trust_tier: decoded but carried no kern id".to_string());
		}
		Err(e) => why.push(format!("pre-trust_tier: {e}")),
	}

	tracing::debug!(
		target: "kern.store",
		candidates = %why.join(" | "),
		"no frozen v10 layout fit this row"
	);
	None
}

/// A cold-tier row as of v10: the same shape as the current one, but the
/// `Entity` inside it is whichever of the two v10 layouts wrote it.
#[derive(Deserialize)]
struct ColdRowV10<E> {
	entity: E,
	temporal: StoredTemporal,
}

impl<E: Into<Entity>> From<ColdRowV10<E>> for crate::ColdRow {
	fn from(c: ColdRowV10<E>) -> Self {
		crate::ColdRow {
			entity: c.entity.into(),
			temporal: c.temporal,
		}
	}
}

/// Decode one already-decompressed v10 cold row, same two candidates as a kern
/// row and the same refusal to guess: an entity with no id is not an entity.
pub(crate) fn decode_v10_cold(raw: &[u8]) -> Option<crate::ColdRow> {
	let cfg = crate::bincode_cfg();
	if let Ok((row, _)) =
		bincode::serde::decode_from_slice::<ColdRowV10<EntityTrustTier>, _>(raw, cfg)
	{
		let row: crate::ColdRow = row.into();
		if !row.entity.id.is_empty() {
			return Some(row);
		}
	}
	if let Ok((row, _)) = bincode::serde::decode_from_slice::<ColdRowV10<Entity>, _>(raw, cfg) {
		let row: crate::ColdRow = row.into();
		if !row.entity.id.is_empty() {
			return Some(row);
		}
	}
	None
}

/// Did that decode produce a kern, or just a shape?
///
/// A successful bincode decode is weaker evidence than it looks: a run of zero
/// bytes reads as empty strings, empty maps and zeroed numbers, which is a
/// structurally valid `Kern` that never existed. Every real kern carries an id
/// — `Kern::new` requires one and the root has a uuid — so an empty id is the
/// cheap tell that these bytes were never this type. Without this, garbage in a
/// row would migrate to an empty kern and the id would be lost with it.
fn plausible(row: &LegacyRow) -> bool {
	!row.kern.id.is_empty()
}

#[cfg(test)]
#[path = "tests/legacy_test.rs"]
mod legacy_tests;
