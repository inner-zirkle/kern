//! The write path: [`accept`] places a new entity in the best-fit kern,
//! [`accept_with_dedup`] first checks the ANN index for a near-duplicate to
//! merge into, and the supersede family stamps replaced revisions and wires
//! the `Supersedes` edges. Everything that adds knowledge to the graph funnels
//! through here so placement, dedup, and bitemporal stamping stay consistent.

use super::graph::GraphGnn;
use super::reason::{add_reason, superseded_ancestors};
use super::search::search_all_unlocked;
use base::base_constants::*;
use base::base_types::*;
use base::crdt::GCounter;
use math::{average_vec, cosine_distance, reason_id};
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
/// order a reconcile produces converges; plain last-writer-wins does not, and
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
/// Stamps a fresh lamport/producer only when the stored deadline actually
/// moves — or when it was never stamped, which is the freshly placed entity
/// carrying its own deadline in.
pub fn merge_valid_until(
	g: &mut GraphGnn,
	entity_id: &str,
	incoming: Option<std::time::SystemTime>,
) -> bool {
	// No incoming retention is `min(t, ∞) = t`: nothing to write.
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
	let producer = g.replica_id.clone();
	let Some(e) = g
		.get_mut(&kern_id)
		.and_then(|k| k.entities.get_mut(entity_id))
	else {
		return false;
	};
	e.valid_until = resolved;
	e.valid_until_lamport = lamport;
	e.valid_until_producer = producer;
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

	let rid = reason_id(entity_id, "", ReasonKind::Rephrase, new_text);
	let reason = Reason {
		id: rid.clone(),
		from: entity_id.to_string(),
		// Rephrase is a LOCAL annotation on `from` — the three cross-kern fields
		// are intentionally blank.
		to: String::new(),
		to_kern_id: String::new(),
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
	let rid = reason_id(from, to, kind, "");
	let reason = Reason {
		id: rid.clone(),
		from: from.to_string(),
		to: to.to_string(),
		to_kern_id: String::new(),
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
pub fn seed_examples(text: &str) -> Vec<String> {
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
pub fn mean_pool(vecs: &[Vec<f32>]) -> Option<Vec<f32>> {
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

pub fn add_graviton_with_mass(g: &mut GraphGnn, name: &str, vec: Vec<f32>, mass: f64) {
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
					&& math::cosine(&c.graviton_vec, vec) >= base::base_constants::GRAVITON_DEDUP_THRESHOLD
			})
			.unwrap_or(false)
	})
}

// Read from the kern map, not the g.root snapshot — runtime mutations land there.
pub fn root_graviton_ids(g: &GraphGnn) -> Vec<String> {
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

pub fn promote_to_root_if_generic(g: &mut GraphGnn, kern_id: &str) -> bool {
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

pub fn remove_graviton(g: &mut GraphGnn, name: &str) -> bool {
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
#[path = "tests/accept_test.rs"]
mod accept_tests;
