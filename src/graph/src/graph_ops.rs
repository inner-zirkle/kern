//! Pure graph operations: forget, link, promote, degrade — the mutations shared
//! by the CLI and MCP surfaces. They take a `GraphGnn` by `&mut` and return
//! counts; the daemon-side wiring (route/load/persist) lives in `commands`.

use crate::graph::{GraphGnn, PendingDelta};
use crate::reason::{add_reason, remove_entity, remove_reason};
use crate::search::find_entity;
use base::base_constants::{
	DEGRADE_DECAY_BASE, DEGRADE_DECAY_POW, DEGRADE_FLOOR, DEGRADE_MIN_THRESHOLD,
};
use base::base_types::{Reason, ReasonKind, ReviewState};
use math::{average_vec, reason_id};
use util::short_id;

#[derive(Default)]
pub struct SourceForget {
	pub removed_entities: usize,
	pub removed_edges: usize,
	// Local Facts the guard refused. Without this a `--source` that hits nothing
	// but Facts prints "forgot 0" and never says why `--force` was the answer.
	pub kept_facts: usize,
}

// `forget_entity`, `forget_by_source`, `link_entities`, `promote_entity`, and
// `degrade_entity_reasons` — each one is the no-I/O mutation the named command
// routes around. The daemon wrapper is responsible for routing; these are
// responsible for what the inspection-snapshot graph actually looks like.
//
// `source_id` hashes scheme+object+section, so keying on one would forget a
// single section of a document and leave the rest. Reaches exactly as far as
// `forget_entity` does — the resident kerns; an unloaded kern is out of reach.

pub fn forget_entity(g: &mut GraphGnn, id: &str, force: bool) -> Result<usize, &'static str> {
	let (thought, kern_id) = find_entity(g, id).ok_or("thought not found")?;
	// A remote Fact is a peer's assertion, not durable local knowledge — forgettable.
	if thought.is_fact() && !force && !crate::merge::is_remote_kern_id(&kern_id) {
		return Err("cannot forget a fact");
	}
	let edges_before = g.kerns.get(&kern_id).map(|k| k.reasons.len()).unwrap_or(0);
	remove_entity(g, &kern_id, id, force);
	let edges_after = g.kerns.get(&kern_id).map(|k| k.reasons.len()).unwrap_or(0);
	// saturating: remove_entity only drops edges, never adds — guard against underflow.
	Ok(edges_before.saturating_sub(edges_after))
}

pub fn forget_by_source(
	g: &mut GraphGnn,
	scheme: &str,
	object_id: &str,
	force: bool,
) -> SourceForget {
	// Collected first: the removal mutates every kern map we would be iterating.
	let ids: Vec<String> = g
		.all()
		.into_iter()
		.flat_map(|k| k.entities.values())
		.filter(|t| t.source.scheme() == scheme && t.source.object_id() == object_id)
		.map(|t| t.id.clone())
		.collect();

	let mut out = SourceForget::default();
	for id in ids {
		match forget_entity(g, &id, force) {
			Ok(edges) => {
				out.removed_entities += 1;
				out.removed_edges += edges;
			}
			Err("cannot forget a fact") => out.kept_facts += 1,
			// The id came out of the graph one statement ago; a miss here means a
			// duplicate id across kerns already took it. Nothing left to remove.
			Err(_) => {}
		}
	}
	out
}

/// Release a held claim. Idempotent: a row already `Active` returns `false`
/// rather than erroring, so a retried promote is not a failure. A typo that
/// resolves to nothing IS one, because silently succeeding would tell a
/// curator the claim was released when it is still held.
pub fn promote_entity(g: &mut GraphGnn, id: &str) -> Result<bool, &'static str> {
	let (thought, kern_id) = find_entity(g, id).ok_or("thought not found")?;
	// Checked on the clone, before `get_mut`: that call bumps the mutation epoch
	// and invalidates the query cache, which a no-op promote has no business doing.
	if thought.review == ReviewState::Active {
		return Ok(false);
	}
	let entity = g
		.get_mut(&kern_id)
		.and_then(|k| k.entities.get_mut(id))
		.ok_or("thought not found")?;
	entity.review = ReviewState::Active;
	Ok(true)
}

/// `score` is the assertion's strength, NOT cosine(from, to): a deliberate link
/// exists precisely to connect what content similarity cannot, so scoring it by
/// endpoint similarity guarantees the edge is weakest exactly where it is the
/// only evidence. Callers pass their source's confidence (user 1.0, agent 0.95).
pub fn link_entities(
	g: &mut GraphGnn,
	from: &str,
	to: &str,
	reason_text: String,
	reason_embed: Option<Vec<f32>>,
	score: f64,
) -> Result<(String, f64), String> {
	let (from_t, from_kern_id) =
		find_entity(g, from).ok_or_else(|| format!("from thought not found: {from}"))?;
	let (to_t, _) = find_entity(g, to).ok_or_else(|| format!("to thought not found: {to}"))?;

	let vec = link_vector(reason_embed, &from_t.vector, &to_t.vector);
	let rid = reason_id(from, to, ReasonKind::Similarity, &reason_text, "");
	let r = Reason {
		id: rid.clone(),
		from: from.to_string(),
		to: to.to_string(),
		kind: ReasonKind::Similarity,
		text: reason_text,
		vector: vec.into(),
		score,
		..Default::default()
	};

	let kern = g.kerns.get_mut(&from_kern_id).ok_or_else(|| {
		format!(
			"link failed: kern {} no longer present",
			short_id(&from_kern_id)
		)
	})?;
	add_reason(kern, r);
	Ok((rid, score))
}

fn link_vector(reason_embed: Option<Vec<f32>>, from_vec: &[f32], to_vec: &[f32]) -> Vec<f32> {
	reason_embed.unwrap_or_else(|| average_vec(from_vec, to_vec))
}

/// Returns (decayed, removed) — how many edges had their score reduced and how
/// many dropped below `DEGRADE_MIN_THRESHOLD` and were removed.
pub fn degrade_entity_reasons(g: &mut GraphGnn, kern_id: &str, id: &str) -> (usize, usize) {
	let rids: Vec<String> = match g.kerns.get(kern_id) {
		Some(kern) => crate::reason::collect_reason_ids(kern, id),
		None => Vec::new(),
	};

	let mut decayed = 0usize;
	let mut removed = 0usize;
	for (i, rid) in rids.iter().enumerate() {
		let decay = DEGRADE_DECAY_BASE * DEGRADE_DECAY_POW.powi(i as i32);

		let should_remove = g
			.kerns
			.get(kern_id)
			.and_then(|kern| kern.reasons.get(rid))
			.map(|r| r.score - decay < DEGRADE_MIN_THRESHOLD)
			.unwrap_or(false);

		if should_remove {
			if let Some(kern) = g.kerns.get_mut(kern_id) {
				remove_reason(kern, rid);
			}
			// A degraded Rephrase takes its alternate wording out of the index with it.
			crate::lexical::reindex_entity(g, kern_id, id);
			removed += 1;
		} else {
			let lamport = g.bump_lamport();
			let producer = g.network_id.clone();
			if let Some(kern) = g.kerns.get_mut(kern_id) {
				if let Some(r) = kern.reasons.get_mut(rid) {
					r.score = (r.score - decay).max(DEGRADE_FLOOR);
					r.score_lamport = lamport;
					r.score_producer = producer.clone();
					let lww_value =
						bincode::serde::encode_to_vec(r.score, bincode::config::standard()).unwrap_or_default();
					g.push_delta(PendingDelta {
						object_id: rid.clone(),
						target: 2,
						replica: String::new(),
						value: 0,
						lamport,
						producer,
						lww_value,
					});
				}
			}
		}
		decayed += 1;
	}
	(decayed, removed)
}

/// A snapshot row for the graviton admin view: name, mass, and the live counts
/// of thoughts and edges it currently pulls in. Rendered as a flat array for
/// the JSON-RPC client.
pub struct GravitonRow {
	pub name: String,
	pub mass: f64,
	pub thoughts: usize,
	pub reasons: usize,
}

pub fn graviton_rows(g: &crate::graph::GraphGnn) -> Vec<GravitonRow> {
	crate::accept::root_graviton_ids(g)
		.iter()
		.filter_map(|cid| g.loaded(cid))
		.map(|c| GravitonRow {
			name: c.graviton_text.clone(),
			mass: c.mass,
			thoughts: c.entities.len(),
			reasons: c.reasons.len(),
		})
		.collect()
}
#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn link_vector_prefers_the_reason_embedding() {
		let v = link_vector(
			Some(vec![1.0, 2.0, 3.0]),
			&[0.0, 0.0, 0.0],
			&[9.0, 9.0, 9.0],
		);
		assert_eq!(
			v,
			vec![1.0, 2.0, 3.0],
			"an embedded reason wins over the midpoint"
		);
	}

	#[test]
	fn link_vector_falls_back_to_endpoint_midpoint() {
		let v = link_vector(None, &[0.0, 2.0], &[4.0, 6.0]);
		assert_eq!(
			v,
			vec![2.0, 4.0],
			"no embedding -> midpoint of the two endpoints"
		);
		assert_eq!(
			v,
			vec![2.0, 4.0],
			"no embedding -> midpoint of the two endpoints"
		);
	}
}
