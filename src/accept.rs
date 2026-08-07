//! The write path: [`accept`] places a new entity in the best-fit kern,
//! [`accept_with_dedup`] first checks the ANN index for a near-duplicate to
//! merge into, and the supersede family stamps replaced revisions and wires
//! the `Supersedes` edges. Everything that adds knowledge to the graph funnels
//! through here so placement, dedup, and bitemporal stamping stay consistent.

use super::graph::GraphGnn;
use math::{average_vec, cosine_distance, reason_id};
use super::reason::{add_reason, superseded_ancestors};
use super::search::search_all_unlocked;
use base::base_constants::*;
use base::base_types::*;
use base::crdt::GCounter;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct AcceptResult {
	pub placed_in: String,
	pub entity_id: String,
	pub deduped: bool,
	pub reason_ids: Vec<String>,
}

const MAX_ACCEPT_DEPTH: usize = 64;
const MASS_EPSILON: f64 = 1e-6;

// Supersede chains that exceeded `SUPERSEDE_CHAIN_HOP_THRESHOLD` on one
// `external_id` — item 58 trigger #1. Process-global like `TRAIN_REFUSED`:
// the count is the only trace that a contested chain ran past the hop budget,
// since the chain itself is bounded by `MAX_ACCEPT_DEPTH` and never errors.
static SUPERSEDE_CHAIN_DEPTH_EXCEEDED: AtomicU64 = AtomicU64::new(0);

pub fn supersede_chain_depth_exceeded() -> u64 {
	SUPERSEDE_CHAIN_DEPTH_EXCEEDED.load(Ordering::Relaxed)
}

// Count the hops behind `old_id` (the existing supersede chain) and bump the
// counter when a new supersede would push the chain past the threshold. Called
// before the new Supersedes edge is added, so `superseded_ancestors(old_id)`
// is the chain as it stood before this hop. `+ 1` is the hop `old_id` itself
// contributes — depth 1 is a first supersede, depth 6 is the sixth.
fn bump_supersede_chain_depth(g: &GraphGnn, old_id: &str) {
	let depth = superseded_ancestors(g, old_id).len() + 1;
	if depth > SUPERSEDE_CHAIN_HOP_THRESHOLD {
		SUPERSEDE_CHAIN_DEPTH_EXCEEDED.fetch_add(1, Ordering::Relaxed);
	}
}

fn effective_distance(dist: f64, mass: f64) -> f64 {
	dist / mass.max(MASS_EPSILON)
}

// Callers with no ingest config in scope (bench, tests) get the same default the
// config layer starts from, so the two dedup checks can never disagree.
pub fn accept(g: &mut GraphGnn, kern_id: &str, thought: Entity, doc_id: &str) -> AcceptResult {
	accept_with_dedup(g, kern_id, thought, doc_id, INGEST_DEDUP_THRESHOLD)
}

pub fn accept_with_dedup(
	g: &mut GraphGnn,
	kern_id: &str,
	thought: Entity,
	doc_id: &str,
	dedup_threshold: f64,
) -> AcceptResult {
	// Dedup scans graph-wide and routing only reads or spawns empty kerns, so
	// the result cannot change during descent — safe to compute once.
	let dup = find_duplicate_hit(g, &thought.vector, dedup_threshold);
	let target_id = route_entity(g, kern_id, &thought, dup.is_some());
	commit_entity(g, &target_id, thought, doc_id, dup)
}

fn find_duplicate_hit(g: &GraphGnn, vector: &[f32], threshold: f64) -> Option<(String, f64)> {
	let h = search_all_unlocked(g, vector, 1).into_iter().next()?;
	(h.score >= threshold).then_some((h.entity_id, h.score))
}

pub struct MergeOutcome {
	pub kern_id: String,
	pub rephrase_id: Option<String>,
	pub same_kind: bool,
}

/// The `valid_until` ceiling rule. A TTL is a bound on how long a statement may
/// live, so merging two bounds keeps the LOWER one; `None` means +∞.
/// `min(∞, t) = t` puts a deadline on a never-expiring entity, and
/// `min(t, ∞) = t` leaves an expiring one alone when the caller expressed no
/// opinion — omitting retention is "no opinion", not "make this permanent".
/// `min` is commutative, associative and idempotent, so the arbitrary replay
/// order federation produces converges; plain last-writer-wins does not, and
/// would let a late near-duplicate carrying 30 days void a deliberate 1 hour.
///
/// KNOWN COST: ingest can therefore only ever SHORTEN a deadline. Lengthening
/// one needs an explicit update path, or `forget` + re-ingest.
pub fn resolve_valid_until(
	current: Option<std::time::SystemTime>,
	incoming: Option<std::time::SystemTime>,
) -> Option<std::time::SystemTime> {
	match (current, incoming) {
		(Some(c), Some(i)) => Some(c.min(i)),
		(Some(c), None) => Some(c),
		(None, i) => i,
	}
}

/// The ONE place a resolved `valid_until` is written. Both dedup gates reach it
/// through `merge_duplicate`; the fresh-placement path in `ingest::place` calls
/// it directly, on the id that actually entered the graph.
///
/// Stamps a fresh lamport/producer and queues the gossip delta only when the
/// stored deadline actually moves — or when it was never stamped, which is the
/// freshly placed entity carrying its own deadline in.
pub fn merge_valid_until(
	g: &mut GraphGnn,
	entity_id: &str,
	incoming: Option<std::time::SystemTime>,
) -> bool {
	// No incoming retention is `min(t, ∞) = t`: nothing to write, nothing to gossip.
	if incoming.is_none() {
		return false;
	}
	let Some(kern_id) = g.kern_of_entity(entity_id).map(str::to_string) else {
		return false;
	};
	let Some((current, stamped)) = g
		.get(&kern_id)
		.and_then(|k| k.entities.get(entity_id))
		.map(|e| (e.valid_until, e.valid_until_lamport > 0))
	else {
		return false;
	};
	let resolved = resolve_valid_until(current, incoming);
	if resolved == current && stamped {
		return false;
	}

	let lamport = g.bump_lamport();
	let producer = g.network_id.clone();
	let Some(e) = g
		.get_mut(&kern_id)
		.and_then(|k| k.entities.get_mut(entity_id))
	else {
		return false;
	};
	e.valid_until = resolved;
	e.valid_until_lamport = lamport;
	e.valid_until_producer = producer.clone();

	let lww_value =
		bincode::serde::encode_to_vec(resolved, bincode::config::standard()).unwrap_or_default();
	g.push_delta(crate::graph::PendingDelta {
		object_id: entity_id.to_string(),
		target: 3,
		replica: String::new(),
		value: 0,
		lamport,
		producer,
		lww_value,
	});
	true
}

// INVARIANT: never overwrite statements/vector under the existing id
// (= content_hash(text)); differing phrasing → Rephrase edge.
pub fn merge_duplicate(
	g: &mut GraphGnn,
	entity_id: &str,
	new_text: &str,
	new_score: f64,
	incoming_kind: EntityKind,
	incoming_valid_until: Option<std::time::SystemTime>,
) -> Option<MergeOutcome> {
	let kern_id = g.kern_of_entity(entity_id)?.to_string();
	// A deduped ingest still carries its retention: the survivor inherits the
	// tighter of the two ceilings. Both dedup gates land here.
	merge_valid_until(g, entity_id, incoming_valid_until);
	let kern = g.get_mut(&kern_id)?;

	let (differs, old_kind) = {
		let t = kern.entities.get_mut(entity_id)?;
		t.observe_support(new_score);
		(t.text() != new_text, t.kind)
	};
	let same_kind = incoming_kind == old_kind;

	if !differs {
		return Some(MergeOutcome {
			kern_id,
			rephrase_id: None,
			same_kind,
		});
	}

	let rid = reason_id(entity_id, "", ReasonKind::Rephrase, new_text, "");
	let reason = Reason {
		id: rid.clone(),
		from: entity_id.to_string(),
		// Rephrase is a LOCAL annotation on `from` — the three cross-kern fields
		// are intentionally blank.
		to: String::new(),
		to_kern_id: String::new(),
		to_net_id: String::new(),
		kind: ReasonKind::Rephrase,
		dirty: false,
		text: new_text.to_string(),
		vector: Embedding::default(),
		score: 0.5,
		score_lamport: 0,
		score_producer: String::new(),
		traversal_count: GCounter::new(),
		producer_id: String::new(),
	};
	add_reason(kern, reason);
	// The wording is stored now; without this it is stored and searchable nowhere.
	crate::lexical::reindex_entity(g, &kern_id, entity_id);

	Some(MergeOutcome {
		kern_id,
		rephrase_id: Some(rid),
		same_kind,
	})
}

fn route_entity(g: &mut GraphGnn, kern_id: &str, thought: &Entity, is_dup: bool) -> String {
	let mut current_id = kern_id.to_string();

	if is_dup {
		return current_id;
	}

	for _depth in 0..MAX_ACCEPT_DEPTH {
		// ponytail: hold &kern.children alongside the &GraphGnn reborrow — both
		// immutable, so the clone that existed only to end a borrow is gone.
		let child_id = {
			let kern = match g.loaded(&current_id) {
				Some(k) => k,
				None => break,
			};
			route_to_child_id(&kern.children, g, &thought.vector)
		};
		if let Some(child_id) = child_id {
			current_id = child_id;
			continue;
		}

		// The root is a pure dispatcher: a no-graviton-match falls through to the
		// `generic` catch-all, never commits onto the root itself.
		if current_id == g.root.id {
			let generic_id = get_or_spawn_generic_child(g, &current_id);
			if generic_id != current_id {
				current_id = generic_id;
				continue;
			}
			break;
		}

		let reject = {
			let kern = match g.loaded(&current_id) {
				Some(k) => k,
				None => break,
			};
			if kern.has_graviton() {
				let dist = effective_distance(
					cosine_distance(&thought.vector, &kern.graviton_vec),
					kern.mass,
				);
				let p = acceptance_probability(dist, kern.inner_radius, kern.outer_radius);
				p < ACCEPT_FLOOR
			} else {
				false
			}
		};

		if reject {
			let child_id = get_or_spawn_unnamed_child(g, &current_id);
			current_id = child_id;
			continue;
		}

		break;
	}
	current_id
}

fn commit_entity(
	g: &mut GraphGnn,
	kern_id: &str,
	mut thought: Entity,
	doc_id: &str,
	dup: Option<(String, f64)>,
) -> AcceptResult {
	// A duplicate MERGES into the survivor: corroboration plus a Rephrase edge for
	// the alternate wording. Returning early stored nothing and merged nothing.
	if let Some((survivor_id, _)) = dup {
		let text = thought.text();
		let outcome = merge_duplicate(
			g,
			&survivor_id,
			&text,
			thought.conf_mean(),
			thought.kind,
			thought.valid_until,
		);
		let (placed_in, reason_ids) = match outcome {
			Some(o) => (o.kern_id, o.rephrase_id.into_iter().collect()),
			None => (kern_id.to_string(), Vec::new()),
		};
		return AcceptResult {
			placed_in,
			entity_id: survivor_id,
			deduped: true,
			reason_ids,
		};
	}

	let root_id = g
		.loaded(kern_id)
		.map(|k| k.root_id.clone())
		.unwrap_or_default();
	thought.root_id = root_id;
	let entity_id = thought.id.clone();
	let thought_vec = thought.vector.clone();
	let external_id = thought.external_id.clone();

	if thought.has_vector() {
		g.entity_idx.insert(entity_id.clone(), thought_vec.clone());
	}

	if let Some(kern) = g.get_mut(kern_id) {
		kern.entities.insert(entity_id.clone(), thought);
	}
	g.index_entity(&entity_id, kern_id);

	let mut reason_ids = Vec::new();

	reason_ids.extend(add_similarity_reason(g, kern_id, &entity_id, &thought_vec));

	reason_ids.extend(add_provenance_reason(
		g,
		kern_id,
		&entity_id,
		&thought_vec,
		doc_id,
	));

	if !external_id.is_empty() {
		let reason_text = g
			.loaded(kern_id)
			.and_then(|k| k.entities.get(&entity_id))
			.map(|e| e.text())
			.unwrap_or_default();
		reason_ids.extend(supersede(
			g,
			kern_id,
			&entity_id,
			&thought_vec,
			&external_id,
			&reason_text,
		));
	}

	AcceptResult {
		placed_in: kern_id.to_string(),
		entity_id,
		deduped: false,
		reason_ids,
	}
}

#[allow(clippy::too_many_arguments)]
fn commit_reason(
	g: &mut GraphGnn,
	kern_id: &str,
	from: &str,
	to: &str,
	kind: ReasonKind,
	score: f64,
	vec: Embedding,
	text: &str,
) -> String {
	let rid = reason_id(from, to, kind, "", "");
	let reason = Reason {
		id: rid.clone(),
		from: from.to_string(),
		to: to.to_string(),
		to_kern_id: String::new(),
		to_net_id: String::new(),
		kind,
		dirty: false,
		text: text.to_string(),
		vector: vec.clone(),
		score,
		score_lamport: 0,
		score_producer: String::new(),
		traversal_count: GCounter::new(),
		producer_id: String::new(),
	};
	if !vec.is_empty() {
		g.reason_idx.insert(rid.clone(), vec);
	}
	if let Some(kern) = g.get_mut(kern_id) {
		add_reason(kern, reason);
	}
	g.index_reason(&rid, kern_id);
	rid
}

fn add_similarity_reason(
	g: &mut GraphGnn,
	kern_id: &str,
	entity_id: &str,
	thought_vec: &[f32],
) -> Vec<String> {
	let hits = search_all_unlocked(g, thought_vec, 2);
	for h in &hits {
		if h.entity_id == entity_id {
			continue;
		}
		let nearest_vec = g
			.kern_of_entity(&h.entity_id)
			.and_then(|kid| g.loaded(kid))
			.and_then(|kern| kern.entities.get(&h.entity_id))
			.map(|t| t.vector.clone())
			.unwrap_or_default();

		let vec = if !thought_vec.is_empty() && !nearest_vec.is_empty() {
			Embedding::from(average_vec(thought_vec, &nearest_vec))
		} else {
			Embedding::default()
		};

		let rid = commit_reason(
			g,
			kern_id,
			entity_id,
			&h.entity_id,
			ReasonKind::Similarity,
			h.score,
			vec,
			"",
		);
		return vec![rid];
	}
	Vec::new()
}

fn add_provenance_reason(
	g: &mut GraphGnn,
	kern_id: &str,
	entity_id: &str,
	thought_vec: &[f32],
	doc_id: &str,
) -> Vec<String> {
	if doc_id.is_empty() {
		return Vec::new();
	}
	let doc_vec = g
		.loaded(kern_id)
		.and_then(|k| k.entities.get(doc_id))
		.filter(|t| t.has_vector())
		.map(|t| t.vector.clone());

	let vec = match (&doc_vec, thought_vec.is_empty()) {
		(Some(dv), false) => Embedding::from(average_vec(thought_vec, dv)),
		_ => Embedding::default(),
	};

	let rid = commit_reason(
		g,
		kern_id,
		entity_id,
		doc_id,
		ReasonKind::Provenance,
		PROVENANCE_SCORE,
		vec,
		"",
	);
	vec![rid]
}

fn supersede(
	g: &mut GraphGnn,
	placed_kern_id: &str,
	entity_id: &str,
	thought_vec: &[f32],
	external_id: &str,
	reason_text: &str,
) -> Vec<String> {
	let index_kern_id = g.kern_of_source(external_id).map(|s| s.to_string());
	let old_id = index_kern_id.as_ref().and_then(|kid| {
		g.loaded(kid)
			.and_then(|k| k.source_index.get(external_id).cloned())
	});

	if old_id.as_deref() == Some(entity_id) {
		return Vec::new();
	}

	if let Some(ref ik) = index_kern_id {
		if ik != placed_kern_id {
			if let Some(kern) = g.get_mut(ik) {
				kern.source_index.remove(external_id);
			}
		}
	}
	if let Some(kern) = g.get_mut(placed_kern_id) {
		kern
			.source_index
			.insert(external_id.to_string(), entity_id.to_string());
	}
	g.set_source_entry(external_id.to_string(), placed_kern_id.to_string());

	let old_id = match old_id {
		Some(id) => id,
		None => return Vec::new(),
	};

	let (old_vec, old_kern_id) = {
		let mut found = None;
		if let Some(ref ik) = index_kern_id {
			if let Some(kern) = g.loaded(ik) {
				if let Some(t) = kern.entities.get(&old_id) {
					found = Some((t.vector.clone(), ik.clone()));
				}
			}
		}
		if found.is_none() {
			// `get` auto-loads the owning kern if it was evicted, so this also
			// finds entities a loaded-only scan would miss.
			if let Some(kid) = g.kern_of_entity(&old_id).map(|s| s.to_string()) {
				if let Some(kern) = g.get(&kid) {
					if let Some(t) = kern.entities.get(&old_id) {
						found = Some((t.vector.clone(), kid));
					}
				}
			}
		}
		match found {
			Some(f) => f,
			None => return Vec::new(),
		}
	};

	// Item 58 trigger #1: count the existing chain behind `old_id` before this
	// hop lands, so a contested chain on one `external_id` is detectable.
	bump_supersede_chain_depth(g, &old_id);

	stamp_superseded(
		g,
		placed_kern_id,
		entity_id,
		thought_vec,
		&old_id,
		&old_kern_id,
		&old_vec,
		reason_text,
	)
}

/// Stamp `old_id` Superseded-by `new_id`, evict it from the ANN indices, and add
/// a `Supersedes` reason edge new→old. Shared by same-external-id `supersede`
/// and cross-external-id `supersede_renamed`.
#[allow(clippy::too_many_arguments)]
fn stamp_superseded(
	g: &mut GraphGnn,
	placed_kern_id: &str,
	entity_id: &str,
	thought_vec: &[f32],
	old_id: &str,
	old_kern_id: &str,
	old_vec: &[f32],
	reason_text: &str,
) -> Vec<String> {
	let now = std::time::SystemTime::now();
	let new_valid_from = g
		.loaded(placed_kern_id)
		.and_then(|k| k.entities.get(entity_id))
		.and_then(|e| e.valid_from_or_created())
		.unwrap_or(now);
	if let Some(kern) = g.get_mut(old_kern_id) {
		if let Some(old) = kern.entities.get_mut(old_id) {
			old.status = EntityStatus::Superseded;
			old.superseded_by = entity_id.to_string();
			old.stamp_invalidated(now, new_valid_from);
		}
	}

	// A superseded entity is never a valid retrieval result — evict from the ANN
	// indices; it stays in `kern.entities` so the supersede chain holds.
	g.entity_idx.delete(old_id);
	g.gnn_entity_idx.delete(old_id);

	// ROADMAP item 60: a deferred contradiction candidate (Rephrase edge on the
	// old entity, `to` empty) is orphaned when the old entity is superseded by a
	// different update than the candidate — `do_classify_contradiction` returns
	// early on `old.is_superseded()`. Re-point the candidate's `from` to the new
	// active entity and queue it for re-classification on the tick loop.
	let mut reclass: Vec<String> = Vec::new();
	if let Some(kern) = g.get_mut(old_kern_id) {
		for r in kern.reasons.values_mut() {
			if r.kind == ReasonKind::Rephrase && r.from == old_id && r.to.is_empty() {
				r.from = entity_id.to_string();
				reclass.push(r.id.clone());
			}
		}
	}
	for rid in reclass {
		g.push_reclass(old_kern_id, &rid);
	}

	let vec = if !thought_vec.is_empty() && !old_vec.is_empty() {
		Embedding::from(average_vec(thought_vec, old_vec))
	} else {
		Embedding::default()
	};

	vec![commit_reason(
		g,
		placed_kern_id,
		entity_id,
		old_id,
		ReasonKind::Supersedes,
		1.0,
		vec,
		reason_text,
	)]
}

/// Supersede the entity that owns `old_external_id` with `new_id`, for a
/// renamed-and-edited file. Unlike `supersede`, this is cross-external-id: the
/// old path is gone, so it is dropped rather than reassigned to the new entity.
/// `source_index` is not populated at plain ingest, so the owner is found by a
/// resident walk — fine for a rare rename event on the (off-by-default) watcher.
pub fn supersede_renamed(
	g: &mut GraphGnn,
	placed_kern_id: &str,
	new_id: &str,
	new_vec: &[f32],
	old_external_id: &str,
	new_external_id: &str,
	reason_text: &str,
) -> Option<String> {
	let mut hit = None;
	for (kid, kern) in g.kerns.iter() {
		for (eid, t) in kern.entities.iter() {
			if t.external_id == old_external_id {
				hit = Some((eid.clone(), kid.clone(), t.vector.to_vec()));
				break;
			}
		}
		if hit.is_some() {
			break;
		}
	}
	let (old_id, old_kern_id, old_vec) = hit?;
	if old_id == new_id {
		// Pure rename: content unchanged, same id. Re-key the survivor's
		// external_id and source-index from the old path to the new path so
		// a `forget --source file://new` resolves and `file://old` does not.
		if let Some(kern) = g.get_mut(&old_kern_id) {
			if let Some(entity) = kern.entities.get_mut(new_id) {
				entity.external_id = new_external_id.to_string();
			}
		}
		if g.kern_of_source(old_external_id).is_some() {
			g.clear_source_entry(old_external_id);
		}
		g.set_source_entry(new_external_id.to_string(), old_kern_id.clone());
		return None;
	}
	// The old path no longer exists; drop its source-keyed entries if any.
	if let Some(kern) = g.get_mut(&old_kern_id) {
		kern.source_index.remove(old_external_id);
	}
	if g.kern_of_source(old_external_id).is_some() {
		g.clear_source_entry(old_external_id);
	}
	stamp_superseded(
		g,
		placed_kern_id,
		new_id,
		new_vec,
		&old_id,
		&old_kern_id,
		&old_vec,
		reason_text,
	);
	Some(old_id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContradictionClass {
	Supersede,
	Related,
}

pub fn classify_prompt(old_text: &str, new_text: &str) -> String {
	format!(
		"Two statements are near-duplicates about the same subject. Decide whether \
the NEW statement UPDATES or CONTRADICTS the OLD one (so the new should replace \
the old), or is merely RELATED (both can coexist). Answer with exactly ONE word: \
UPDATE, CONTRADICTION, or RELATED.\n\nOLD: {old_text}\nNEW: {new_text}\n"
	)
}

// Fails open to Related (any RELATED mention wins) — the conservative choice.
pub fn parse_contradiction(raw: &str) -> ContradictionClass {
	let up = raw.trim().to_uppercase();
	let supersede = up.contains("CONTRADICT") || up.contains("UPDATE");
	if supersede && !up.contains("RELATED") {
		ContradictionClass::Supersede
	} else {
		ContradictionClass::Related
	}
}

pub fn supersede_by_contradiction(
	g: &mut GraphGnn,
	kern_id: &str,
	old_id: &str,
	new_thought: Entity,
	reason_text: &str,
) -> Vec<String> {
	let new_id = new_thought.id.clone();
	if new_id == old_id {
		return Vec::new();
	}
	let old_kern_id = match g.kern_of_entity(old_id).map(str::to_string) {
		Some(k) => k,
		None => return Vec::new(),
	};
	let (old_vec, already_superseded) =
		match g.loaded(&old_kern_id).and_then(|k| k.entities.get(old_id)) {
			Some(o) => (o.vector.clone(), o.is_superseded()),
			None => return Vec::new(),
		};
	if already_superseded {
		return Vec::new();
	}

	let new_vec = new_thought.vector.clone();
	let new_valid_from = new_thought
		.valid_from_or_created()
		.unwrap_or_else(std::time::SystemTime::now);
	let root_id = g
		.loaded(kern_id)
		.map(|k| k.root_id.clone())
		.unwrap_or_default();

	let mut new_thought = new_thought;
	new_thought.root_id = root_id;
	if new_thought.has_vector() {
		g.entity_idx.insert(new_id.clone(), new_vec.clone());
	}
	if let Some(kern) = g.get_mut(kern_id) {
		kern.entities.insert(new_id.clone(), new_thought);
	}
	g.index_entity(&new_id, kern_id);

	let now = std::time::SystemTime::now();
	if let Some(kern) = g.get_mut(&old_kern_id) {
		if let Some(old) = kern.entities.get_mut(old_id) {
			old.status = EntityStatus::Superseded;
			old.superseded_by = new_id.clone();
			old.stamp_invalidated(now, new_valid_from);
		}
	}
	g.entity_idx.delete(old_id);
	g.gnn_entity_idx.delete(old_id);

	let vec = if !new_vec.is_empty() && !old_vec.is_empty() {
		Embedding::from(average_vec(&new_vec, &old_vec))
	} else {
		Embedding::default()
	};
	// Item 58 trigger #1: same chain-depth measure as the same-external-id
	// `supersede` path — a contradiction supersede is another hop on the chain.
	bump_supersede_chain_depth(g, old_id);
	vec![commit_reason(
		g,
		kern_id,
		&new_id,
		old_id,
		ReasonKind::Supersedes,
		1.0,
		vec,
		reason_text,
	)]
}

pub fn get_or_spawn_unnamed_child(g: &mut GraphGnn, kern_id: &str) -> String {
	// Use `get` (auto-loads), NOT `loaded`: an evicted child would otherwise be
	// respawned every call — the runaway that filled the graph with unnamed kerns.
	let children = g
		.get(kern_id)
		.map(|k| k.children.clone())
		.unwrap_or_default();
	for child_id in &children {
		if let Some(c) = g.get(child_id) {
			if c.is_unnamed() {
				return child_id.clone();
			}
		}
	}
	spawn_unnamed_child(g, kern_id)
}

// Always creates a FRESH unnamed child (one per call). For the single reusable
// holding-pen child use get_or_spawn_unnamed_child.
pub fn spawn_unnamed_child(g: &mut GraphGnn, kern_id: &str) -> String {
	let root_id = g
		.get(kern_id)
		.map(|k| k.root_id.clone())
		.unwrap_or_default();
	let child = Kern::new_unnamed(kern_id, &root_id);
	let child_id = child.id.clone();
	g.register(child);
	if let Some(kern) = g.get_mut(kern_id) {
		kern.children.push(child_id.clone());
	}
	child_id
}

// The generic catch-all: empty graviton_vec never matches routing; named, hence immortal.
pub(crate) fn get_or_spawn_generic_child(g: &mut GraphGnn, parent_id: &str) -> String {
	// Use `get` (auto-loads), NOT `loaded`: even the immortal generic child can
	// spill to disk — same duplicate-spawn runaway as get_or_spawn_unnamed_child.
	let children = g
		.get(parent_id)
		.map(|k| k.children.clone())
		.unwrap_or_default();
	for child_id in &children {
		if let Some(c) = g.get(child_id) {
			if c.graviton_text == GENERIC_GRAVITON {
				return child_id.clone();
			}
		}
	}
	let root_id = g
		.get(parent_id)
		.map(|k| k.root_id.clone())
		.unwrap_or_default();
	let child = Kern::new_named_child(parent_id, &root_id, GENERIC_GRAVITON, Vec::new());
	let child_id = child.id.clone();
	g.register(child);
	if let Some(kern) = g.get_mut(parent_id) {
		kern.children.push(child_id.clone());
	}
	child_id
}

/// A multi-line graviton seed is a list of example statements, one per line.
/// Measured (2026-07-21, qwen3-embedding:0.6b): the mean of per-example
/// embeddings sits ~0.39 median cosine distance from held-out claims of the
/// same focus, vs ~0.55 for an abstract description and ~0.55-0.61 for the
/// same examples embedded as one concatenated blob. Pooling separate embeds
/// is the win; concatenation muddies it.
pub(crate) fn seed_examples(text: &str) -> Vec<String> {
	let lines: Vec<String> = text
		.lines()
		.map(str::trim)
		.filter(|l| !l.is_empty())
		.map(str::to_string)
		.collect();
	if lines.len() < 2 {
		let whole = text.trim();
		if whole.chars().count() > base::base_constants::GRAVITON_SEED_CHAR_CHUNK {
			// ponytail: char-budget split on a code-point boundary; the caller
			// embeds each chunk and mean_pools them, same as the multi-line path.
			let mut out = Vec::new();
			let mut buf = String::new();
			let mut budget = base::base_constants::GRAVITON_SEED_CHAR_CHUNK;
			for ch in whole.chars() {
				buf.push(ch);
				budget -= 1;
				if budget == 0 {
					out.push(std::mem::take(&mut buf));
					budget = base::base_constants::GRAVITON_SEED_CHAR_CHUNK;
				}
			}
			if !buf.is_empty() {
				out.push(buf);
			}
			out
		} else {
			vec![whole.to_string()]
		}
	} else {
		lines
	}
}

/// Normalized mean of the example embeddings. Empty input or mismatched
/// dimensions yield None — the caller falls back to a single whole-text embed.
pub(crate) fn mean_pool(vecs: &[Vec<f32>]) -> Option<Vec<f32>> {
	let first = vecs.first()?;
	let dim = first.len();
	if dim == 0 || vecs.iter().any(|v| v.len() != dim) {
		return None;
	}
	let n = vecs.len() as f32;
	let mut mean: Vec<f32> = vec![0.0; dim];
	for v in vecs {
		for (m, x) in mean.iter_mut().zip(v) {
			*m += x / n;
		}
	}
	let norm = mean.iter().map(|x| x * x).sum::<f32>().sqrt();
	if norm == 0.0 {
		return None;
	}
	for m in &mut mean {
		*m /= norm;
	}
	Some(mean)
}

pub(crate) fn add_graviton_with_mass(g: &mut GraphGnn, name: &str, vec: Vec<f32>, mass: f64) {
	if let Some(existing) = find_graviton_by_name(g, name) {
		if let Some(k) = g.get_mut(&existing) {
			k.graviton_vec = vec;
			k.mass = mass;
		}
		return;
	}
	let root = g.root.id.clone();
	let root_net = g.root.root_id.clone();
	let mut child = Kern::new_named_child(&root, &root_net, name, vec);
	child.mass = mass;
	let cid = child.id.clone();
	g.register(child);
	if let Some(r) = g.get_mut(&root) {
		if !r.children.contains(&cid) {
			r.children.push(cid);
		}
	}
}

/// Promote an existing unnamed kern to named by giving it a graviton in place
/// — no move, no id change, no re-register. The kern keeps its entities, children
/// and parent; it just becomes `is_named` (and so is kept by gc, not reaped as a
/// transient spill child). ROADMAP item 84: `kern unnamed` used to list only.
pub fn promote_unnamed(
	g: &mut GraphGnn,
	kern_id: &str,
	name: &str,
	vec: Vec<f32>,
	mass: f64,
) -> Result<(), String> {
	let parent = g.loaded(kern_id).map(|k| k.parent.clone());
	let is_unnamed = g.loaded(kern_id).map(|k| k.is_unnamed()).unwrap_or(false);
	if parent.is_none() || !is_unnamed {
		return Err(format!("no unnamed kern with id {kern_id}"));
	}
	if !vec.is_empty() {
		if let Some(k) = g.get_mut(kern_id) {
			k.graviton_text = name.to_string();
			k.graviton_vec = vec;
			k.mass = mass;
		}
		return Ok(());
	}
	Err("empty graviton vector".into())
}

fn find_graviton_by_name(g: &GraphGnn, name: &str) -> Option<String> {
	let needle = name.trim().to_lowercase();
	root_graviton_ids(g).into_iter().find(|cid| {
		g.loaded(cid)
			.map(|c| c.graviton_text.trim().to_lowercase() == needle)
			.unwrap_or(false)
	})
}

fn equivalent_graviton_exists(g: &GraphGnn, name: &str, vec: &[f32]) -> bool {
	if find_graviton_by_name(g, name).is_some() {
		return true;
	}
	if vec.is_empty() {
		return false;
	}
	root_graviton_ids(g).into_iter().any(|cid| {
		g.loaded(&cid)
			.map(|c| {
				!c.graviton_vec.is_empty()
					&& math::cosine(&c.graviton_vec, vec)
						>= base::base_constants::GRAVITON_DEDUP_THRESHOLD
			})
			.unwrap_or(false)
	})
}

// Read from the kern map, not the g.root snapshot — runtime mutations land there.
pub(crate) fn root_graviton_ids(g: &GraphGnn) -> Vec<String> {
	let root = g.root.id.clone();
	let children = g
		.loaded(&root)
		.map(|r| r.children.clone())
		.unwrap_or_default();
	children
		.into_iter()
		.filter(|cid| {
			g.loaded(cid)
				.map(|c| !c.graviton_text.is_empty() && c.graviton_text != GENERIC_GRAVITON)
				.unwrap_or(false)
		})
		.collect()
}

pub(crate) fn promote_to_root_if_generic(g: &mut GraphGnn, kern_id: &str) -> bool {
	let parent_id = match g.loaded(kern_id) {
		Some(k) => k.parent.clone(),
		None => return false,
	};
	let under_generic = g
		.loaded(&parent_id)
		.map(|p| p.graviton_text == GENERIC_GRAVITON)
		.unwrap_or(false);
	if !under_generic {
		return false;
	}
	let (cand_name, cand_vec) = match g.loaded(kern_id) {
		Some(k) => (k.graviton_text.clone(), k.graviton_vec.clone()),
		None => return false,
	};
	if equivalent_graviton_exists(g, &cand_name, &cand_vec) {
		return false;
	}
	let root_id = g.root.id.clone();
	if let Some(gen_kern) = g.get_mut(&parent_id) {
		gen_kern.children.retain(|c| c.as_str() != kern_id);
	}
	if let Some(k) = g.get_mut(kern_id) {
		k.parent = root_id.clone();
	}
	if let Some(root) = g.get_mut(&root_id) {
		if !root.children.iter().any(|c| c.as_str() == kern_id) {
			root.children.push(kern_id.to_string());
		}
	}
	true
}

pub(crate) fn remove_graviton(g: &mut GraphGnn, name: &str) -> bool {
	let root = g.root.id.clone();
	let generic = get_or_spawn_generic_child(g, &root);
	let target = root_graviton_ids(g).into_iter().find(|cid| {
		*cid != generic
			&& g
				.loaded(cid)
				.map(|c| c.graviton_text == name)
				.unwrap_or(false)
	});
	let Some(tid) = target else {
		return false;
	};
	if let Some(t) = g.get_mut(&tid) {
		t.graviton_text.clear();
		t.graviton_vec.clear();
		t.parent = generic.clone();
	}
	if let Some(r) = g.get_mut(&root) {
		r.children.retain(|c| c != &tid);
	}
	if let Some(gk) = g.get_mut(&generic) {
		gk.children.push(tid);
	}
	true
}

fn route_to_child_id(children: &[String], g: &GraphGnn, vec: &[f32]) -> Option<String> {
	let mut best_id = None;
	let mut best_p = 0.0;
	let mut best_d = f64::MAX;
	for id in children {
		let c = match g.loaded(id) {
			Some(k) if k.is_named() && !k.graviton_vec.is_empty() => k,
			_ => continue,
		};
		let dist = effective_distance(cosine_distance(vec, &c.graviton_vec), c.mass);
		let p = acceptance_probability(dist, c.inner_radius, c.outer_radius);
		// The probability saturates at 1.0 inside the inner radius, so ties
		// there are real; effective distance breaks them, keeping mass
		// meaningful when several gravitons all fully accept.
		if p > best_p || (p == best_p && dist < best_d) {
			best_p = p;
			best_d = dist;
			best_id = Some(id.clone());
		}
	}
	if best_p < ACCEPT_FLOOR {
		return None;
	}
	best_id
}

pub fn acceptance_probability(dist: f64, inner: f64, outer: f64) -> f64 {
	if dist <= inner {
		1.0
	} else if dist >= outer {
		0.0
	} else {
		let x = (dist - inner) / (outer - inner);
		1.0 / (1.0 + (8.0 * (x - 0.5)).exp())
	}
}

// `SUPERSEDE_CHAIN_DEPTH_EXCEEDED` is process-global; a test that moves it
// must hold this while any test measures it, same lesson as `TRAIN_REFUSED`
// (`src/tick/trainer.rs`). `std::sync::Mutex` rather than tokio because every
// holder is a plain `#[test]`.
#[cfg(test)]
pub(crate) static SUPERSEDE_CHAIN_TEST_MUX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
	use super::*;
	use crate::graph::GraphGnn;

	fn ent(id: &str, vector: Vec<f32>) -> Entity {
		Entity {
			id: id.into(),
			vector: vector.into(),
			statements: vec!["x".into()],
			..Default::default()
		}
	}

	#[test]
	fn unnamed_child_reused_when_evicted_by_load_cap() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			crate::base_store::Store::open(&g.data_dir).unwrap(),
		));
		g.set_max_loaded_kerns(1);
		let root = g.root.id.clone();

		let first = get_or_spawn_unnamed_child(&mut g, &root);
		for _ in 0..20 {
			let id = get_or_spawn_unnamed_child(&mut g, &root);
			assert_eq!(id, first, "must reuse the evicted unnamed child");
		}
		assert_eq!(g.count(), 2, "no runaway kern creation under the cap");
	}

	#[test]
	fn generic_child_reused_when_evicted_by_load_cap() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			crate::base_store::Store::open(&g.data_dir).unwrap(),
		));
		g.set_max_loaded_kerns(1);
		let root = g.root.id.clone();

		let first = get_or_spawn_generic_child(&mut g, &root);
		for _ in 0..20 {
			let id = get_or_spawn_generic_child(&mut g, &root);
			assert_eq!(id, first, "must reuse the evicted generic child");
		}
		assert_eq!(
			g.count(),
			2,
			"exactly one generic child created, no runaway"
		);
	}

	#[test]
	fn unnamed_child_not_duplicated_when_non_root_parent_evicts() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			crate::base_store::Store::open(&g.data_dir).unwrap(),
		));
		g.set_max_loaded_kerns(1);
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();

		let parent = {
			let p = Kern::new_named_child(&root, &root_net, "parent-graviton", vec![1.0, 0.0]);
			let pid = p.id.clone();
			g.register(p);
			if let Some(r) = g.get_mut(&root) {
				if !r.children.contains(&pid) {
					r.children.push(pid.clone());
				}
			}
			pid
		};

		let first = get_or_spawn_unnamed_child(&mut g, &parent);
		for _ in 0..20 {
			let id = get_or_spawn_unnamed_child(&mut g, &parent);
			assert_eq!(
				id, first,
				"reuse the unnamed child even when the non-root parent evicted"
			);
		}
		assert_eq!(
			g.count(),
			3,
			"no runaway: root + parent + one unnamed child"
		);
	}

	// ROADMAP item 83: the cluster path uses `spawn_unnamed_child` (always a
	// distinct child), unlike `get_or_spawn_unnamed_child` (reuses). Under a cap,
	// `register` inside `spawn_unnamed_child` can evict the parent before its
	// `children` list gains the new id. This pins that the parent's persisted
	// `children` survives the eviction — no re-spawn loop, no fragmentation.
	#[test]
	fn spawn_unnamed_child_under_cap_keeps_the_child_in_parent_children() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			crate::base_store::Store::open(&g.data_dir).unwrap(),
		));
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();
		// cap = 2 so registering a child under a non-root parent forces eviction
		g.set_max_loaded_kerns(2);
		let parent = {
			let p = Kern::new_named_child(&root, &root_net, "parent-graviton", vec![1.0, 0.0]);
			let pid = p.id.clone();
			g.register(p);
			if let Some(r) = g.get_mut(&root) {
				if !r.children.contains(&pid) {
					r.children.push(pid.clone());
				}
			}
			pid
		};
		let child = spawn_unnamed_child(&mut g, &parent);
		// reload the parent from the store — the eviction inside `register` may
		// have unloaded it, so `loaded` alone could miss the persisted children.
		let reloaded_children = g
			.get(&parent)
			.map(|k| k.children.clone())
			.unwrap_or_default();
		assert!(
			reloaded_children.contains(&child),
			"the new child must be in the parent's persisted children after eviction: got {reloaded_children:?}"
		);
		assert_eq!(
			g.count(),
			3,
			"root + parent + one child, no re-spawn runaway"
		);
	}

	// ROADMAP item 60: superseding an entity that carries a deferred Rephrase
	// candidate re-points the candidate to the new active entity and queues it
	// for re-classification, so it is not orphaned by `do_classify_contradiction`'s
	// `old.is_superseded()` early return.
	#[test]
	fn supersede_repoints_a_deferred_rephrase_to_the_new_entity() {
		use crate::reason::add_reason;
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let mut old = Entity {
			id: "old".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			..Default::default()
		};
		old.statements = vec!["old claim".into()];
		g.get_mut(&kid).unwrap().entities.insert("old".into(), old);
		g.get_mut(&kid)
			.unwrap()
			.source_index
			.insert("ext1".into(), "old".into());
		g.index_entity("old", &kid);
		// a deferred contradiction candidate: Rephrase on `old`, `to` empty
		let rid = reason_id("old", "", ReasonKind::Rephrase, "rephrased wording", "");
		add_reason(
			g.get_mut(&kid).unwrap(),
			Reason {
				id: rid.clone(),
				from: "old".into(),
				to: String::new(),
				kind: ReasonKind::Rephrase,
				text: "rephrased wording".into(),
				..Default::default()
			},
		);
		g.set_source_entry("ext1".into(), kid.clone());

		supersede(&mut g, &kid, "new", &[1.0, 0.0], "ext1", "replaced");

		// the candidate is re-pointed to `new` and queued for re-classification
		let kern = g.loaded(&kid).unwrap();
		let r = kern.reasons.get(&rid).expect("rephrase edge kept");
		assert_eq!(r.from, "new", "re-pointed to the new active entity");
		assert!(r.to.is_empty(), "still a deferred candidate");
		let queued = g.drain_pending_reclass();
		assert!(
			queued.iter().any(|(k, r)| k == &kid && r == &rid),
			"queued for re-classification: {queued:?}"
		);
	}

	#[test]
	fn supersede_drops_the_old_entity_from_the_search_index() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let old = Entity {
			id: "old".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			..Default::default()
		};
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("old".into(), old);
			k.source_index.insert("ext1".into(), "old".into());
		}
		g.index_entity("old", &kid);
		g.set_source_entry("ext1".into(), kid.clone());

		let before: Vec<String> = search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			before.contains(&"old".to_string()),
			"old is indexed before supersede"
		);

		supersede(
			&mut g,
			&kid,
			"new",
			&[1.0, 0.0],
			"ext1",
			"replaced by newer version",
		);

		let after: Vec<String> = search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			!after.contains(&"old".to_string()),
			"superseded entity removed from search index"
		);
		let kern = g.loaded(&kid).unwrap();
		let old_e = kern
			.entities
			.get("old")
			.expect("superseded entity still stored");
		assert_eq!(
			old_e.status,
			EntityStatus::Superseded,
			"kept as Superseded history"
		);
		assert_eq!(old_e.superseded_by, "new", "supersede chain preserved");
	}

	#[test]
	fn accept_never_leaves_empty_unnamed_kern() {
		let (mut g, root, _graviton) = graph_with_graviton();
		let vectors = [
			vec![1.0, 0.0, 0.0], // matches the graviton
			vec![1.0, 0.0, 0.0], // duplicate -> deduped, must NOT spawn
			vec![0.0, 1.0, 0.0], // non-match -> generic
			vec![0.0, 1.0, 0.0], // duplicate of the generic one
			vec![0.0, 0.0, 1.0], // another non-match
			vec![0.9, 0.1, 0.0], // near the graviton
		];
		for (i, v) in vectors.iter().enumerate() {
			accept(&mut g, &root, ent(&format!("e{i}"), v.clone()), "");
		}
		let empties: Vec<String> = g
			.all()
			.iter()
			.filter(|k| k.id != root && k.is_unnamed() && k.entities.is_empty())
			.map(|k| k.id.clone())
			.collect();
		assert!(
			empties.is_empty(),
			"accept left empty unnamed kern(s) behind: {empties:?}"
		);
	}

	#[test]
	fn supersede_chain_depth_counter_increments_past_threshold() {
		let _serial = SUPERSEDE_CHAIN_TEST_MUX.lock().unwrap();
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		// Seed `ext1` with e0, then supersede six times: e1←e0, e2←e1, … e6←e5.
		// The sixth hop makes superseded_ancestors(e5) = [e4,e3,e2,e1,e0] (len 5),
		// depth 6 > SUPERSEDE_CHAIN_HOP_THRESHOLD (5) → one increment.
		let old = Entity {
			id: "e0".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			..Default::default()
		};
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("e0".into(), old);
			k.source_index.insert("ext1".into(), "e0".into());
		}
		g.set_source_entry("ext1".into(), kid.clone());
		g.index_entity("e0", &kid);

		let before = supersede_chain_depth_exceeded();
		// Insert e_i then supersede(e_i): `supersede` does not insert the new
		// entity, so the next hop's `old` lookup needs it present in the kern.
		for i in 1..=6 {
			let new_id = format!("e{i}");
			if let Some(k) = g.get_mut(&kid) {
				k.entities.insert(
					new_id.clone(),
					Entity {
						id: new_id.clone(),
						external_id: "ext1".into(),
						vector: vec![1.0, 0.0].into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
			g.index_entity(&new_id, &kid);
			supersede(&mut g, &kid, &new_id, &[1.0, 0.0], "ext1", "hop");
		}
		let delta = supersede_chain_depth_exceeded() - before;
		assert_eq!(
			delta, 1,
			"a 6-deep chain on one external_id increments the counter once"
		);

		// A fresh 3-deep chain on a different external_id stays under threshold.
		let before2 = supersede_chain_depth_exceeded();
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert(
				"s0".into(),
				Entity {
					id: "s0".into(),
					external_id: "ext2".into(),
					vector: vec![0.0, 1.0].into(),
					status: EntityStatus::Active,
					..Default::default()
				},
			);
			k.source_index.insert("ext2".into(), "s0".into());
		}
		g.set_source_entry("ext2".into(), kid.clone());
		g.index_entity("s0", &kid);
		for i in 1..=3 {
			let new_id = format!("s{i}");
			if let Some(k) = g.get_mut(&kid) {
				k.entities.insert(
					new_id.clone(),
					Entity {
						id: new_id.clone(),
						external_id: "ext2".into(),
						vector: vec![0.0, 1.0].into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
			g.index_entity(&new_id, &kid);
			supersede(&mut g, &kid, &new_id, &[0.0, 1.0], "ext2", "hop");
		}
		let delta2 = supersede_chain_depth_exceeded() - before2;
		assert_eq!(delta2, 0, "a 3-deep chain does not cross the threshold");
	}

	#[test]
	fn supersede_stamps_both_temporal_clocks() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let old = Entity {
			id: "old".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			created_at: Some(std::time::SystemTime::now()),
			..Default::default()
		};
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		let new_from = std::time::SystemTime::now();
		let new = Entity {
			id: "new".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			valid_from: Some(new_from),
			..Default::default()
		};
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("old".into(), old);
			k.entities.insert("new".into(), new);
			k.source_index.insert("ext1".into(), "old".into());
		}
		g.index_entity("old", &kid);
		g.index_entity("new", &kid);
		g.set_source_entry("ext1".into(), kid.clone());

		supersede(&mut g, &kid, "new", &[1.0, 0.0], "ext1", "temporal test");

		let kern = g.loaded(&kid).unwrap();
		let old_e = kern.entities.get("old").unwrap();
		assert_eq!(old_e.status, EntityStatus::Superseded);
		assert!(
			old_e.invalidated_at.is_some(),
			"transaction-time stamp recorded"
		);
		assert_eq!(
			old_e.valid_to,
			Some(new_from),
			"old window closes at the successor's valid_from"
		);
		assert!(
			!old_e.is_valid_at(new_from),
			"old is no longer valid at the successor's start instant"
		);
	}

	#[test]
	fn contradiction_supersede_materializes_new_and_invalidates_old() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let old = Entity {
			id: "old".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			created_at: Some(std::time::SystemTime::now()),
			..Default::default()
		};
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("old".into(), old);
		}
		g.index_entity("old", &kid);

		let new = Entity {
			id: "new".into(),
			vector: vec![0.99, 0.01].into(),
			status: EntityStatus::Active,
			created_at: Some(std::time::SystemTime::now()),
			..Default::default()
		};
		let rids = supersede_by_contradiction(&mut g, &kid, "old", new, "contradicts earlier claim");
		assert_eq!(rids.len(), 1, "one Supersedes edge minted");

		let kern = g.loaded(&kid).unwrap();
		let sup_r = kern.reasons.get(&rids[0]).expect("supersede reason exists");
		assert_eq!(
			sup_r.text, "contradicts earlier claim",
			"reason text stored"
		);

		let kern = g.loaded(&kid).unwrap();
		assert!(
			kern.entities.contains_key("new"),
			"new revision materialized"
		);
		let old_e = kern.entities.get("old").unwrap();
		assert_eq!(old_e.status, EntityStatus::Superseded);
		assert_eq!(old_e.superseded_by, "new");
		assert!(old_e.invalidated_at.is_some(), "old stamped invalidated");

		let hits: Vec<String> = search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(!hits.contains(&"old".to_string()), "old evicted from ANN");
		assert!(hits.contains(&"new".to_string()), "new revision indexed");
	}

	#[test]
	fn contradiction_supersede_is_a_noop_on_missing_or_already_superseded() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let new = Entity {
			id: "new".into(),
			vector: vec![1.0, 0.0].into(),
			..Default::default()
		};
		assert!(supersede_by_contradiction(&mut g, &kid, "ghost", new, "missing old").is_empty());
	}

	#[test]
	fn parse_contradiction_fails_open_to_related() {
		assert_eq!(parse_contradiction("UPDATE"), ContradictionClass::Supersede);
		assert_eq!(
			parse_contradiction("  contradiction \n"),
			ContradictionClass::Supersede
		);
		assert_eq!(parse_contradiction("RELATED"), ContradictionClass::Related);
		assert_eq!(parse_contradiction(""), ContradictionClass::Related);
		assert_eq!(
			parse_contradiction("I'm not sure"),
			ContradictionClass::Related
		);
		assert_eq!(
			parse_contradiction("this is an update but they are RELATED"),
			ContradictionClass::Related,
			"a RELATED mention wins — conservative"
		);
	}

	#[test]
	fn resolve_valid_until_is_a_min_with_none_as_infinity() {
		use std::time::{Duration, UNIX_EPOCH};
		let early = UNIX_EPOCH + Duration::from_secs(100);
		let late = UNIX_EPOCH + Duration::from_secs(500);

		assert_eq!(resolve_valid_until(Some(late), Some(early)), Some(early));
		assert_eq!(
			resolve_valid_until(Some(early), Some(late)),
			Some(early),
			"commutative — the shorter deadline wins in either order"
		);
		assert_eq!(
			resolve_valid_until(None, Some(early)),
			Some(early),
			"min(∞, t) = t — a never-expiring entity accepts a deadline"
		);
		assert_eq!(
			resolve_valid_until(Some(early), None),
			Some(early),
			"min(t, ∞) = t — no opinion never lengthens a deadline"
		);
		assert_eq!(resolve_valid_until(None, None), None);
		assert_eq!(
			resolve_valid_until(Some(early), Some(early)),
			Some(early),
			"idempotent"
		);
	}

	// The incremental sibling of `rebuild_index_shares_the_map_s_vector_allocation`:
	// `commit_entity` indexes on insert rather than waiting for a rebuild, and it
	// has to hand the index the entity's own allocation too or a live graph pays
	// the second copy back one entity at a time.
	#[test]
	fn commit_entity_indexes_the_entity_s_own_vector_allocation() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let r = accept(&mut g, &root, ent("a", vec![1.0, 0.0, 0.0]), "");
		assert!(
			!r.deduped,
			"the fixture only holds while the entity is placed"
		);
		let kid = g.kern_of_entity(&r.entity_id).expect("indexed").to_string();
		let e = &g.loaded(&kid).expect("kern").entities[&r.entity_id];
		assert_eq!(
			std::sync::Arc::strong_count(&e.vector),
			2,
			"entity_idx must share the committed entity's vector, not copy it"
		);
	}

	#[test]
	fn duplicate_vector_is_deduped() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let r1 = accept(&mut g, &root, ent("a", vec![1.0, 0.0, 0.0]), "");
		assert!(!r1.deduped, "first entity is placed, not deduped");
		let r2 = accept(&mut g, &root, ent("b", vec![1.0, 0.0, 0.0]), "");
		assert!(r2.deduped, "identical vector must dedup");
	}

	fn ent_text(id: &str, vector: Vec<f32>, text: &str) -> Entity {
		Entity {
			id: id.into(),
			vector: vector.into(),
			statements: vec![text.into()],
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::StatementRef,
				index: 0,
				text: String::new(),
			}],
			..Default::default()
		}
	}

	fn survivor<'a>(g: &'a GraphGnn, id: &str) -> &'a Entity {
		let kid = g.kern_of_entity(id).expect("survivor is indexed");
		g.loaded(kid).unwrap().entities.get(id).unwrap()
	}

	#[test]
	fn accept_time_duplicate_merges_instead_of_dropping() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let r1 = accept(
			&mut g,
			&root,
			ent_text("a", vec![1.0, 0.0, 0.0], "the claim"),
			"",
		);
		assert!(!r1.deduped);
		let before = survivor(&g, "a").clone();

		let r2 = accept(
			&mut g,
			&root,
			ent_text("b", vec![1.0, 0.0, 0.0], "the claim reworded"),
			"",
		);
		assert!(r2.deduped, "identical vector must dedup");
		assert_eq!(
			r2.entity_id, "a",
			"result names the SURVIVOR, not the dropped incoming id"
		);

		let after = survivor(&g, "a");
		assert!(
			after.conf_alpha > before.conf_alpha,
			"duplicate corroborates the survivor instead of vanishing"
		);
		assert!(after.updated_at.is_some(), "updated_at bumped");
		assert!(
			!g.loaded(&r2.placed_in).unwrap().entities.contains_key("b"),
			"the duplicate is not stored under its own id"
		);
	}

	#[test]
	fn accept_time_duplicate_records_rephrase_edge() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		accept(
			&mut g,
			&root,
			ent_text("a", vec![1.0, 0.0, 0.0], "the claim"),
			"",
		);
		let r = accept(
			&mut g,
			&root,
			ent_text("b", vec![1.0, 0.0, 0.0], "the claim reworded"),
			"",
		);

		let kid = g.kern_of_entity("a").unwrap();
		let rephrase: Vec<_> = g
			.loaded(kid)
			.unwrap()
			.reasons
			.values()
			.filter(|x| x.kind == ReasonKind::Rephrase)
			.collect();
		assert_eq!(rephrase.len(), 1, "exactly one rephrase edge");
		assert_eq!(rephrase[0].from, "a", "annotated on the survivor");
		assert_eq!(
			rephrase[0].text, "the claim reworded",
			"alternate phrasing preserved"
		);
		assert_eq!(
			r.reason_ids,
			vec![rephrase[0].id.clone()],
			"merge reports the edge it minted"
		);
	}

	#[test]
	fn accept_time_merge_never_overwrites_survivor_text_or_vector() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		accept(
			&mut g,
			&root,
			ent_text("a", vec![1.0, 0.0, 0.0], "the claim"),
			"",
		);
		let before = survivor(&g, "a").clone();

		accept(
			&mut g,
			&root,
			ent_text("b", vec![1.0, 0.0, 0.0], "a totally different wording"),
			"",
		);

		let after = survivor(&g, "a");
		assert_eq!(after.id, "a", "content-addressed id unchanged");
		assert_eq!(after.text(), before.text(), "stored text NOT overwritten");
		assert_eq!(
			after.statements, before.statements,
			"statements NOT overwritten"
		);
		assert_eq!(after.vector, before.vector, "vector NOT overwritten");
	}

	#[test]
	fn distinct_vector_is_placed() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		accept(&mut g, &root, ent("a", vec![1.0, 0.0, 0.0]), "");
		let r = accept(&mut g, &root, ent("c", vec![0.0, 1.0, 0.0]), "");
		assert!(!r.deduped, "orthogonal vector must not dedup");
	}

	fn graph_with_graviton() -> (GraphGnn, String, String) {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();
		let graviton = Kern::new_named_child(&root, &root_net, "work", vec![1.0, 0.0, 0.0]);
		let graviton_id = graviton.id.clone();
		g.register(graviton);
		g.get_mut(&root).unwrap().children.push(graviton_id.clone());
		(g, root, graviton_id)
	}

	#[test]
	fn routes_nonmatch_to_generic() {
		let (mut g, root, graviton_id) = graph_with_graviton();
		let r = accept(&mut g, &root, ent("e", vec![0.0, 1.0, 0.0]), "");
		assert_ne!(
			r.placed_in, root,
			"must not commit onto the root dispatcher"
		);
		assert_ne!(
			r.placed_in, graviton_id,
			"non-matching entity must not enter the graviton"
		);
		let placed = g.loaded(&r.placed_in).expect("placed kern is loaded");
		assert_eq!(
			placed.graviton_text, GENERIC_GRAVITON,
			"fell through to generic"
		);
	}

	#[test]
	fn routes_match_to_graviton() {
		let (mut g, root, graviton_id) = graph_with_graviton();
		let r = accept(&mut g, &root, ent("e", vec![1.0, 0.0, 0.0]), "");
		assert_eq!(
			r.placed_in, graviton_id,
			"matching entity enters its graviton"
		);
	}

	// ponytail: the per-descent children clone is gone — routing through a root
	// with 4 named children allocates no more than through a root with 1, since
	// `&kern.children` is held alongside the `&GraphGnn` reborrow (item 31).
	#[test]
	fn route_entity_does_not_clone_children_per_descent() {
		use crate::test_support::alloc_probe;

		let build = |n: usize| -> (GraphGnn, String) {
			let mut g = GraphGnn::new();
			let root = g.root.id.clone();
			let root_net = g.root.root_id.clone();
			for i in 0..n {
				let name = format!("work{i}");
				let mut v = vec![0.0, 0.0, 0.0];
				v[i % 3] = 1.0;
				let k = Kern::new_named_child(&root, &root_net, &name, v);
				g.get_mut(&root).unwrap().children.push(k.id.clone());
				g.register(k);
			}
			(g, root)
		};

		let thought = ent("e", vec![1.0, 0.0, 0.0]);
		let (mut g4, root4) = build(4);
		let (mut g1, root1) = build(1);

		let (_, a4) = alloc_probe::measure(|| route_entity(&mut g4, &root4, &thought, false));
		let (_, a1) = alloc_probe::measure(|| route_entity(&mut g1, &root1, &thought, false));
		// The matched-id String clone is the only alloc left and it is the same
		// length in both; the children Vec<String> clone is gone, so the two agree
		// within a tight tolerance. A re-added `.clone()` of 4 vs 1 children would
		// push a4 roughly 3 String-headers (~72 B) past a1 and red this.
		let diff = a4.total as i64 - a1.total as i64;
		assert!(
			diff.abs() <= 8,
			"children clone leaked: a4={a4:?} a1={a1:?} diff={diff}"
		);
	}

	fn graviton_names(g: &GraphGnn) -> Vec<String> {
		root_graviton_ids(g)
			.iter()
			.filter_map(|c| g.loaded(c))
			.map(|k| k.graviton_text.clone())
			.collect()
	}

	#[test]
	fn add_graviton_creates_named_root_child() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		assert!(graviton_names(&g).contains(&"work".to_string()));
		let r = accept(&mut g, &root, ent("e", vec![1.0, 0.0, 0.0]), "");
		assert!(
			g.loaded(&r.placed_in)
				.map(|k| k.graviton_text == "work")
				.unwrap_or(false),
			"matching entity enters the added graviton"
		);
	}

	#[test]
	fn remove_graviton_demotes_and_reports() {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		assert!(remove_graviton(&mut g, "work"), "existing graviton removed");
		assert!(
			!graviton_names(&g).contains(&"work".to_string()),
			"graviton no longer a named root child"
		);
		assert!(
			!remove_graviton(&mut g, "missing"),
			"missing graviton -> false"
		);
	}

	#[test]
	fn promote_skips_when_root_has_equivalent_graviton_by_name() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "sessions with no parent", vec![1.0, 0.0, 0.0], 1.0);
		let generic = get_or_spawn_generic_child(&mut g, &root);
		let root_net = g.root.root_id.clone();
		let child = Kern::new_named_child(
			&generic,
			&root_net,
			" Sessions With No Parent ",
			vec![0.0, 1.0, 0.0],
		);
		let cid = child.id.clone();
		g.register(child);
		g.get_mut(&generic).unwrap().children.push(cid.clone());

		assert!(
			!promote_to_root_if_generic(&mut g, &cid),
			"name-equivalent graviton exists -> no promotion"
		);
		assert!(
			!root_graviton_ids(&g).contains(&cid),
			"not minted as a root graviton"
		);
		assert_eq!(
			g.loaded(&cid).unwrap().parent,
			generic,
			"stays under generic"
		);
	}

	#[test]
	fn promote_skips_when_root_graviton_vec_is_near_duplicate() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "parentless sessions", vec![1.0, 0.0, 0.0], 1.0);
		let generic = get_or_spawn_generic_child(&mut g, &root);
		let root_net = g.root.root_id.clone();

		let near = Kern::new_named_child(
			&generic,
			&root_net,
			"sessions without parents",
			vec![1.0, 0.1, 0.0],
		);
		let near_id = near.id.clone();
		g.register(near);
		g.get_mut(&generic).unwrap().children.push(near_id.clone());
		assert!(
			!promote_to_root_if_generic(&mut g, &near_id),
			"vector-equivalent graviton exists -> no promotion"
		);

		let fresh = Kern::new_named_child(&generic, &root_net, "shader pipelines", vec![0.0, 0.0, 1.0]);
		let fresh_id = fresh.id.clone();
		g.register(fresh);
		g.get_mut(&generic).unwrap().children.push(fresh_id.clone());
		assert!(
			promote_to_root_if_generic(&mut g, &fresh_id),
			"orthogonal concept still promotes"
		);
	}

	#[test]
	fn heavier_graviton_wins_at_equal_distance() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "light", vec![1.0, 0.0, 0.0], 1.0);
		add_graviton_with_mass(&mut g, "heavy", vec![0.0, 1.0, 0.0], 2.0);

		let r = accept(&mut g, &root, ent("e", vec![1.0, 1.0, 0.0]), "");
		assert_eq!(
			g.loaded(&r.placed_in).unwrap().graviton_text,
			"heavy",
			"equal cosine distance, larger mass -> smaller effective distance -> wins"
		);
	}

	#[test]
	fn default_mass_preserves_nearest_graviton_routing() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "near", vec![1.0, 0.0, 0.0], 1.0);
		add_graviton_with_mass(&mut g, "far", vec![0.0, 1.0, 0.0], 1.0);
		for id in root_graviton_ids(&g) {
			assert_eq!(g.loaded(&id).unwrap().mass, 1.0, "default mass is 1.0");
		}

		let r = accept(&mut g, &root, ent("e", vec![0.95, 0.05, 0.0]), "");
		assert_eq!(
			g.loaded(&r.placed_in).unwrap().graviton_text,
			"near",
			"mass 1.0 everywhere reproduces plain nearest-distance routing"
		);
	}

	#[test]
	fn seed_examples_splits_lines_and_keeps_single_text_whole() {
		assert_eq!(
			seed_examples("one example.\n  two example.  \n\nthree."),
			vec!["one example.", "two example.", "three."]
		);
		assert_eq!(
			seed_examples("a single description with no newlines"),
			vec!["a single description with no newlines"]
		);
		assert_eq!(
			seed_examples("  padded single line  \n"),
			vec!["padded single line"],
			"one non-empty line embeds whole, not as a one-element pool"
		);
	}

	#[test]
	fn seed_examples_char_chunks_a_long_single_paragraph() {
		let chunk = base::base_constants::GRAVITON_SEED_CHAR_CHUNK;
		let body = "x".repeat(chunk + 5);
		let out = seed_examples(&body);
		assert_eq!(out.len(), 2, "ceil((chunk+5)/chunk) -> 2 chunks");
		assert!(out.iter().all(|c| c.chars().count() <= chunk));
		assert_eq!(
			out.concat(),
			body,
			"chunks reassemble to the trimmed original"
		);
		// exactly-on-boundary: chunk chars -> one chunk (not two)
		assert_eq!(seed_examples(&"x".repeat(chunk)).len(), 1);
	}

	#[test]
	fn seed_examples_char_chunks_split_on_a_code_point_boundary() {
		// a multibyte char straddling the boundary must not be split mid-char
		let chunk = base::base_constants::GRAVITON_SEED_CHAR_CHUNK;
		let mut body = "a".repeat(chunk - 1);
		body.push('ß');
		body.push('z');
		let out = seed_examples(&body);
		// ß is one char, so chunk-1 'a' + 'ß' fills chunk 1; 'z' is chunk 2
		assert_eq!(out.len(), 2);
		assert_eq!(out.concat(), body);
		assert!(out.iter().all(|c| c.chars().count() <= chunk));
	}

	#[test]
	fn mean_pool_normalizes_and_rejects_mismatched_dims() {
		let v = mean_pool(&[vec![1.0, 0.0], vec![0.0, 1.0]]).unwrap();
		let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
		assert!((norm - 1.0).abs() < 1e-6, "pooled vector is unit-norm");
		assert!((v[0] - v[1]).abs() < 1e-6, "equal contribution");
		assert!(mean_pool(&[]).is_none());
		assert!(mean_pool(&[vec![1.0, 0.0], vec![1.0]]).is_none());
		assert!(
			mean_pool(&[vec![1.0, 0.0], vec![-1.0, 0.0]]).is_none(),
			"opposite examples cancel to zero — refuse rather than emit garbage"
		);
	}

	// ROADMAP item 84: `promote_unnamed` gives an existing unnamed kern a
	// graviton in place — no move, no id change, no re-register — so it becomes
	// `is_named` and gc keeps it.
	#[test]
	fn promote_unnamed_adds_a_graviton_in_place() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();
		let child = Kern::new_unnamed(&root, &root_net);
		let cid = child.id.clone();
		g.register(child);
		assert!(
			g.loaded(&cid).unwrap().is_unnamed(),
			"precondition: unnamed"
		);

		promote_unnamed(&mut g, &cid, "pinned", vec![1.0, 0.0], 2.0).unwrap();

		let k = g.loaded(&cid).unwrap();
		assert!(k.is_named(), "now named (has a graviton)");
		assert!(k.has_graviton(), "text + vec set");
		assert_eq!(k.graviton_text, "pinned");
		assert_eq!(k.mass, 2.0);
		assert_eq!(k.id, cid, "no id change");
		assert_eq!(k.parent, root, "no move");
	}

	#[test]
	fn promote_unnamed_rejects_a_named_or_missing_kern() {
		let mut g = GraphGnn::new();
		// missing
		assert!(promote_unnamed(&mut g, "ghost", "x", vec![1.0, 0.0], 1.0).is_err());
		// already named
		add_graviton_with_mass(&mut g, "docs", vec![1.0, 0.0, 0.0], 1.0);
		let id = find_graviton_by_name(&g, "docs").unwrap();
		assert!(
			promote_unnamed(&mut g, &id, "dup", vec![1.0, 0.0], 1.0).is_err(),
			"a named kern is not a promote target"
		);
	}

	#[test]
	fn add_graviton_with_mass_round_trips_and_updates_in_place() {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "docs", vec![1.0, 0.0, 0.0], 3.0);
		let id = find_graviton_by_name(&g, "docs").unwrap();
		assert_eq!(g.loaded(&id).unwrap().mass, 3.0, "mass stored on add");

		add_graviton_with_mass(&mut g, "docs", vec![0.0, 1.0, 0.0], 0.5);
		assert_eq!(
			g.loaded(&id).unwrap().mass,
			0.5,
			"same-name add updates mass in place"
		);
	}

	#[test]
	fn add_graviton_updates_existing_same_name_instead_of_minting_duplicate() {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		add_graviton_with_mass(&mut g, "work", vec![0.0, 1.0, 0.0], 1.0);

		let ids: Vec<String> = root_graviton_ids(&g)
			.into_iter()
			.filter(|cid| {
				g.loaded(cid)
					.map(|c| c.graviton_text == "work")
					.unwrap_or(false)
			})
			.collect();
		assert_eq!(ids.len(), 1, "one graviton per name, not one per call");
		let vec = g.loaded(&ids[0]).unwrap().graviton_vec.clone();
		assert_eq!(
			vec,
			vec![0.0, 1.0, 0.0],
			"second call updates the routing vector in place"
		);
	}

	#[test]
	fn promotes_generic_child_to_root() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let generic = get_or_spawn_generic_child(&mut g, &root);
		let root_net = g.root.root_id.clone();
		let child = Kern::new_named_child(&generic, &root_net, "shaders", vec![1.0, 0.0, 0.0]);
		let cid = child.id.clone();
		g.register(child);
		g.get_mut(&generic).unwrap().children.push(cid.clone());

		assert!(
			promote_to_root_if_generic(&mut g, &cid),
			"promoted out of generic"
		);
		assert!(
			root_graviton_ids(&g).contains(&cid),
			"now a root-level graviton"
		);
		assert_eq!(
			g.loaded(&cid).unwrap().parent,
			root,
			"parent rewired to root"
		);
		assert!(
			!g.loaded(&generic).unwrap().children.contains(&cid),
			"detached from generic"
		);
		assert!(
			!promote_to_root_if_generic(&mut g, &cid),
			"idempotent once at root level"
		);
	}
}
