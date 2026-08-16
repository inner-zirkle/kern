//! Raw ANN search over the whole graph: entity and reason k-NN with the
//! dimension guard in front (an off-model query vector is dropped and counted,
//! never scored as noise). Retrieval's seeding and fusion build on these hits;
//! this layer knows vectors, not ranking policy.

use std::sync::atomic::{AtomicU64, Ordering};

use super::graph::GraphGnn;
use super::hnsw::HnswHit;
use base::base_types::{Entity, Reason};
use util::cmp_rank;
use util::LogThrottle;

const DIM_WARN_SECS: u64 = 60;
static DIM_REJECTED: AtomicU64 = AtomicU64::new(0);
static DIM_WARN: LogThrottle = LogThrottle::new(DIM_WARN_SECS);

// Queries dropped by the dimension guard since this process opened. A dropped
// query returns nothing, so the count is its only trace.
pub fn query_dim_rejected() -> u64 {
	DIM_REJECTED.load(Ordering::Relaxed)
}

// cosine() truncates to the shorter side, so a query embedded by a different
// model scores as noise and ranks that noise as if it were recall. Recall is
// fail-open — this degrades to "no hits", never a panic — but a silent no-op is
// what let the mismatch hide, so it is counted and (throttled) logged.
fn dim_guard(g: &GraphGnn, vec: &[f32]) -> bool {
	if g.query_dim_ok(vec) {
		return true;
	}
	let total = DIM_REJECTED.fetch_add(1, Ordering::Relaxed) + 1;
	if DIM_WARN.allow() {
		tracing::warn!(
			target: "kern.search",
			query_dim = vec.len(),
			index_dim = g.entity_vector_dim().unwrap_or(0),
			total_rejected = total,
			"query vector dimension disagrees with the indexed dimension — returning no hits; \
			 re-embed with the stored model or run `kern reembed` (further rejections counted, not logged)"
		);
	}
	false
}

#[derive(Debug, Clone)]
pub struct EntityHit {
	pub entity_id: String,
	pub score: f64,
}

impl From<(String, f64)> for EntityHit {
	fn from((entity_id, score): (String, f64)) -> Self {
		Self { entity_id, score }
	}
}

#[derive(Debug, Clone)]
pub struct ReasonHit {
	pub reason_id: String,
	pub score: f64,
}

// Blend weights for a node found in both indices; must sum to 1.0.
const CONTENT_BLEND: f64 = 0.4;
const GNN_BLEND: f64 = 0.6;

fn merge_hits(primary: Vec<HnswHit>, gnn: Vec<HnswHit>, k: usize) -> Vec<EntityHit> {
	use std::collections::hash_map::Entry;
	let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
	for h in primary {
		scores.insert(h.id, h.score);
	}
	for h in gnn {
		match scores.entry(h.id) {
			// Presence in the content map — not the score's sign — decides the blend
			// (scores are cosine in [-1, 1]); do not gate on score > 0.
			Entry::Occupied(mut e) => {
				let blended = CONTENT_BLEND * *e.get() + GNN_BLEND * h.score;
				e.insert(blended);
			}
			Entry::Vacant(e) => {
				e.insert(h.score);
			}
		}
	}
	if scores.is_empty() {
		return Vec::new();
	}
	let mut ranked: Vec<_> = scores.into_iter().collect();
	// Score desc, id-asc tiebreak — deterministic over HashMap order, so truncate(k) is reproducible.
	ranked.sort_by(|a, b| cmp_rank(a.1, &a.0, b.1, &b.0));
	ranked.truncate(k);
	ranked.into_iter().map(EntityHit::from).collect()
}

pub fn search_all_unlocked(g: &GraphGnn, vec: &[f32], k: usize) -> Vec<EntityHit> {
	if vec.is_empty() || !dim_guard(g, vec) {
		return Vec::new();
	}
	let ef = (k * 2).max(64);
	let primary = if g.entity_idx.is_empty() {
		Vec::new()
	} else {
		g.entity_idx.search(vec, k, ef)
	};
	let gnn = if g.gnn_entity_idx.is_empty() {
		Vec::new()
	} else {
		g.gnn_entity_idx.search(vec, k, ef)
	};
	merge_hits(primary, gnn, k)
}

pub fn search_all_filtered(
	g: &GraphGnn,
	vec: &[f32],
	k: usize,
	keep: &dyn Fn(&str) -> bool,
) -> Vec<EntityHit> {
	if vec.is_empty() || !dim_guard(g, vec) {
		return Vec::new();
	}
	let ef = (k * 2).max(64);
	let primary = if g.entity_idx.is_empty() {
		Vec::new()
	} else {
		g.entity_idx.search_filtered(vec, k, ef, keep)
	};
	let gnn = if g.gnn_entity_idx.is_empty() {
		Vec::new()
	} else {
		g.gnn_entity_idx.search_filtered(vec, k, ef, keep)
	};
	merge_hits(primary, gnn, k)
}

pub fn search_reasons_all_unlocked(g: &GraphGnn, vec: &[f32], k: usize) -> Vec<ReasonHit> {
	if g.reason_idx.is_empty() || vec.is_empty() || !dim_guard(g, vec) {
		return Vec::new();
	}
	let ef = (k * 2).max(64);
	g.reason_idx
		.search(vec, k, ef)
		.into_iter()
		.map(|h| ReasonHit {
			reason_id: h.id,
			score: h.score,
		})
		.collect()
}

pub fn find_entity(g: &GraphGnn, id: &str) -> Option<(Entity, String)> {
	if let Some(kid) = g.kern_of_entity(id) {
		if let Some(kern) = g.loaded(kid) {
			if let Some(t) = kern.entities.get(id) {
				return Some((t.clone(), kern.id.clone()));
			}
		}
	}
	for kern in g.all() {
		if let Some(t) = kern.entities.get(id) {
			return Some((t.clone(), kern.id.clone()));
		}
	}
	for kern in g.all() {
		if let Some(r) = kern.refs.get(id) {
			if let Some(ref_kern) = g.loaded(&r.kern_id) {
				if let Some(t) = ref_kern.entities.get(&r.entity_id) {
					return Some((t.clone(), ref_kern.id.clone()));
				}
			}
		}
	}
	None
}

// Exact first, then a unique-enough prefix: every id kern prints is shortened
// (`short_id`), so a copied id is normally a prefix. Lives here rather than in
// the CLI because the daemon's id lookup has to accept exactly what the CLI
// accepts — a routed read that resolved fewer ids than the local one would
// trade staleness for a miss.
pub fn find_entity_by_prefix(g: &GraphGnn, id: &str) -> Option<(Entity, String)> {
	if let Some(pair) = find_entity(g, id) {
		return Some(pair);
	}
	for k in g.all() {
		for t in k.entities.values() {
			if t.id.starts_with(id) {
				return Some((t.clone(), k.id.clone()));
			}
		}
	}
	None
}

pub fn find_reason(g: &GraphGnn, id: &str) -> Option<(Reason, String)> {
	if let Some(kid) = g.kern_of_reason(id) {
		if let Some(kern) = g.loaded(kid) {
			if let Some(r) = kern.reasons.get(id) {
				return Some((r.clone(), kern.id.clone()));
			}
		}
	}
	for kern in g.all() {
		if let Some(r) = kern.reasons.get(id) {
			return Some((r.clone(), kern.id.clone()));
		}
	}
	None
}

#[cfg(test)]
#[path = "tests/search_test.rs"]
mod search_tests;
