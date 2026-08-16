//! Reason-edge upkeep: add/remove edges with their `by_from`/`by_to` index
//! maintenance, walk supersede ancestry, and move or remove entities with
//! their incident edges — the low-level graph surgery `accept` and the
//! commands build on.

use super::graph::GraphGnn;
use base::base_types::{Kern, Reason, ReasonKind};
use std::collections::HashSet;

pub fn collect_reason_ids(kern: &Kern, entity_id: &str) -> Vec<String> {
	let mut ids = Vec::new();
	if let Some(from_ids) = kern.by_from.get(entity_id) {
		ids.extend(from_ids.iter().cloned());
	}
	if let Some(to_ids) = kern.by_to.get(entity_id) {
		ids.extend(to_ids.iter().cloned());
	}
	ids
}

// Supersedes edges point new -> old; walk outgoing. `seen` terminates cycles.
pub fn superseded_ancestors(g: &GraphGnn, entity_id: &str) -> Vec<String> {
	let mut out = Vec::new();
	let mut seen: HashSet<String> = HashSet::new();
	let mut frontier = vec![entity_id.to_string()];
	while let Some(cur) = frontier.pop() {
		let Some(kid) = g.kern_of_entity(&cur).map(str::to_string) else {
			continue;
		};
		let Some(kern) = g.loaded(&kid) else {
			continue;
		};
		let Some(edges) = kern.by_from.get(&cur) else {
			continue;
		};
		for rid in edges {
			if let Some(r) = kern.reasons.get(rid) {
				if r.kind == ReasonKind::Supersedes && !r.to.is_empty() && seen.insert(r.to.clone()) {
					out.push(r.to.clone());
					frontier.push(r.to.clone());
				}
			}
		}
	}
	out
}

pub fn add_reason(kern: &mut Kern, reason: Reason) {
	let id = reason.id.clone();
	let from = reason.from.clone();
	let to = reason.to.clone();
	// Index adjacency only for NEW ids: `by_from`/`by_to` are Vecs, so re-adding
	// the same edge id would append a duplicate and leave a stale entry on remove.
	let is_new = kern.reasons.insert(id.clone(), reason).is_none();
	if !is_new {
		return;
	}
	kern.by_from.entry(from).or_default().push(id.clone());
	if !to.is_empty() {
		kern.by_to.entry(to).or_default().push(id);
	}
}

pub fn remove_reason(kern: &mut Kern, id: &str) {
	let reason = match kern.reasons.remove(id) {
		Some(r) => r,
		None => return,
	};
	remove_string_from_vec(kern.by_from.get_mut(&reason.from), id);
	if !reason.to.is_empty() {
		remove_string_from_vec(kern.by_to.get_mut(&reason.to), id);
	}
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum MoveError {
	#[error("kern not found: {0}")]
	KernNotFound(String),
	#[error("entity {entity} not found in kern {kern}")]
	EntityNotFound { kern: String, entity: String },
}

// A kern hosts a reason iff it hosts its `from`: OUTGOING reasons move, incoming stay.
//
// Every fallible lookup resolves BEFORE the first mutation: a rejected move leaves
// the graph byte-identical. Once validated, `&mut g` is exclusive, so the mutation
// phase cannot observe a missing kern.
pub fn move_entity(
	g: &mut GraphGnn,
	from_kern_id: &str,
	to_kern_id: &str,
	entity_id: &str,
) -> Result<(), MoveError> {
	let src = g
		.kerns
		.get(from_kern_id)
		.ok_or_else(|| MoveError::KernNotFound(from_kern_id.to_string()))?;
	if !src.entities.contains_key(entity_id) {
		return Err(MoveError::EntityNotFound {
			kern: from_kern_id.to_string(),
			entity: entity_id.to_string(),
		});
	}
	if !g.kerns.contains_key(to_kern_id) {
		return Err(MoveError::KernNotFound(to_kern_id.to_string()));
	}
	if from_kern_id == to_kern_id {
		return Ok(());
	}

	let src = g.kerns.get_mut(from_kern_id).expect("validated above");
	let entity = src.entities.remove(entity_id).expect("validated above");
	let (outgoing_rids, incoming_rids) = reasons_touching(src, entity_id);

	for rid in &incoming_rids {
		if let Some(reason) = src.reasons.get_mut(rid) {
			if reason.to_kern_id.is_empty() {
				reason.to_kern_id = to_kern_id.to_string();
			}
		}
	}

	let mut moved_reasons = Vec::with_capacity(outgoing_rids.len());
	for rid in &outgoing_rids {
		if let Some(reason) = src.reasons.remove(rid) {
			remove_string_from_vec(src.by_from.get_mut(&reason.from), rid);
			if !reason.to.is_empty() {
				remove_string_from_vec(src.by_to.get_mut(&reason.to), rid);
			}
			moved_reasons.push(reason);
		}
	}

	let dst = g.kerns.get_mut(to_kern_id).expect("validated above");
	let moved_ids: Vec<String> = moved_reasons.iter().map(|r| r.id.clone()).collect();
	for mut reason in moved_reasons {
		if !reason.to.is_empty() && reason.to != entity_id && reason.to_kern_id.is_empty() {
			reason.to_kern_id = from_kern_id.to_string();
		}
		add_reason(dst, reason);
	}
	dst.entities.insert(entity_id.to_string(), entity);

	g.index_entity(entity_id, to_kern_id);
	for rid in &moved_ids {
		g.index_reason(rid, to_kern_id);
	}
	// Direct `kerns` mutation — same epoch contract as `remove_entity`.
	g.bump_mutation_epoch();
	Ok(())
}

// Active LOCAL Facts are immune; Superseded facts are not. Missing id is a silent no-op.
//
// `force` is the ONE deliberate bypass of that immunity (ROADMAP item 19): a
// legal deletion of the source outranks GC-immunity. It exists here and not
// only in `forget_entity` because this is where the removal actually happens —
// a caller that punched through the outer guard alone would report a removal
// this function silently refused. Every other caller passes false.
pub fn remove_entity(g: &mut GraphGnn, kern_id: &str, id: &str, force: bool) {
	let kern = match g.kerns.get_mut(kern_id) {
		Some(k) => k,
		None => return,
	};

	if let Some(t) = kern.entities.get(id) {
		// A SUPERSEDED fact is invalidated history, not durable knowledge — the
		// bi-temporal GC spills it to the cold tier and drops it here.
		if !force && t.is_fact() && !t.is_superseded() {
			return;
		}
	}
	if kern.entities.remove(id).is_none() {
		return;
	}

	let (outgoing, incoming) = reasons_touching(kern, id);
	let rids: Vec<String> = outgoing.into_iter().chain(incoming).collect();
	for rid in &rids {
		remove_reason(kern, rid);
	}
	kern.by_from.remove(id);
	kern.by_to.remove(id);

	for rid in &rids {
		g.reason_idx.delete(rid);
		g.unindex_reason(rid);
	}

	g.entity_idx.delete(id);
	g.gnn_entity_idx.delete(id);
	g.unindex_entity(id);

	if let Some(lex) = g.lexical() {
		lex.remove(id);
	}
	// This path mutates through `kerns` directly, never `get_mut` — anything
	// memoized on the mutation epoch (the daemon's query cache) must still see
	// a forget, or a deleted thought keeps being served from cache.
	g.bump_mutation_epoch();
}

// A self-loop counts once, as outgoing.
fn reasons_touching(kern: &Kern, entity_id: &str) -> (Vec<String>, Vec<String>) {
	let outgoing: Vec<String> = kern.by_from.get(entity_id).cloned().unwrap_or_default();
	let mut incoming = Vec::new();
	if let Some(to_rids) = kern.by_to.get(entity_id) {
		for rid in to_rids {
			if !outgoing.contains(rid) {
				incoming.push(rid.clone());
			}
		}
	}
	(outgoing, incoming)
}

// Linear scan intentional: the serde-persisted `Vec` is a format change to swap.
fn remove_string_from_vec(vec: Option<&mut Vec<String>>, s: &str) {
	if let Some(v) = vec {
		if let Some(pos) = v.iter().position(|x| x == s) {
			v.remove(pos);
		}
	}
}

#[cfg(test)]
#[path = "tests/reason_test.rs"]
mod reason_tests;
