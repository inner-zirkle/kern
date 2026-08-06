use std::sync::atomic::{AtomicU64, Ordering};

use super::graph::GraphGnn;
use super::hnsw::HnswHit;
use super::log_throttle::LogThrottle;
use crate::base_types::{Entity, Reason};
use super::util::cmp_rank;

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
mod tests {
	use super::*;

	fn populated() -> GraphGnn {
		let mut g = GraphGnn::new();
		for i in 0..60 {
			let x = (i as f64 * 0.3).sin() as f32;
			let y = (i as f64 * 0.3).cos() as f32;
			let z = (i % 5) as f32 * 0.2;
			g.entity_idx.insert(format!("e{i}"), vec![x, y, z].into());
		}
		g
	}

	// Entities live in a kern (not just the index) so the graph can report an
	// indexed dimension — that is what the guard compares against.
	fn indexed(dim: usize) -> GraphGnn {
		use crate::base_types::{Entity, Kern};
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut k = Kern::new("k1", &root);
		for i in 0..8 {
			let id = format!("e{i}");
			let mut v = vec![0.1_f32; dim];
			v[i % dim] = 1.0;
			k.entities.insert(
				id.clone(),
				Entity {
					id,
					vector: v.into(),
					..Default::default()
				},
			);
		}
		g.register(k);
		g.rebuild_index();
		g
	}

	#[test]
	fn a_query_of_the_wrong_dimension_is_a_counted_no_op() {
		let g = indexed(4);
		assert!(
			!search_all_unlocked(&g, &[1.0, 0.1, 0.1, 0.1], 5).is_empty(),
			"the matching dimension still searches"
		);

		let before = query_dim_rejected();
		assert!(
			search_all_unlocked(&g, &[1.0, 0.1, 0.1], 5).is_empty(),
			"a 3-dim query against a 4-dim index returns nothing, not truncated noise"
		);
		assert!(search_all_filtered(&g, &[1.0, 0.1, 0.1], 5, &|_| true).is_empty());
		assert!(
			query_dim_rejected() >= before + 2,
			"a fail-open no-op is still counted"
		);
	}

	fn even(id: &str) -> bool {
		id.trim_start_matches('e')
			.parse::<usize>()
			.map(|n| n % 2 == 0)
			.unwrap_or(false)
	}

	fn hh(id: &str, score: f64) -> HnswHit {
		HnswHit {
			id: id.into(),
			score,
		}
	}

	#[test]
	fn merge_blends_a_nonpositive_content_hit_present_in_both() {
		let primary = vec![hh("z", 0.0), hh("n", -0.4)];
		let gnn = vec![hh("z", 0.5), hh("n", 0.5)];
		let out = merge_hits(primary, gnn, 10);
		let score_of = |id: &str| out.iter().find(|h| h.entity_id == id).map(|h| h.score);
		assert_eq!(
			score_of("z"),
			Some(CONTENT_BLEND * 0.0 + GNN_BLEND * 0.5),
			"zero-sim content still blends"
		);
		assert_eq!(
			score_of("n"),
			Some(CONTENT_BLEND * -0.4 + GNN_BLEND * 0.5),
			"negative-sim content still blends"
		);
	}

	#[test]
	fn merge_keeps_single_index_hits_and_blends_shared_positive() {
		let out = merge_hits(
			vec![hh("c", 0.9), hh("both", 0.8)],
			vec![hh("g", 0.7), hh("both", 0.6)],
			10,
		);
		let score_of = |id: &str| out.iter().find(|h| h.entity_id == id).map(|h| h.score);
		assert_eq!(score_of("c"), Some(0.9), "content-only kept");
		assert_eq!(score_of("g"), Some(0.7), "gnn-only kept");
		assert_eq!(
			score_of("both"),
			Some(CONTENT_BLEND * 0.8 + GNN_BLEND * 0.6),
			"shared blends"
		);
	}

	#[test]
	fn search_all_filtered_returns_only_matching_ids() {
		let g = populated();
		let q = vec![0.0_f32.sin(), 0.0_f32.cos(), 0.0];
		let hits = search_all_filtered(&g, &q, 10, &even);
		assert!(!hits.is_empty(), "filtered search finds matches");
		assert!(
			hits.iter().all(|h| even(&h.entity_id)),
			"every returned id passes the predicate"
		);
	}

	#[test]
	fn search_all_filtered_reject_all_is_empty() {
		let g = populated();
		assert!(search_all_filtered(&g, &[1.0, 0.0, 0.0], 5, &|_| false).is_empty());
	}

	#[test]
	fn search_reasons_ranks_by_proximity_and_guards_empty() {
		let mut g = GraphGnn::new();
		g.reason_idx.insert("r_x".into(), vec![1.0, 0.0].into());
		g.reason_idx.insert("r_y".into(), vec![0.0, 1.0].into());

		let hits = search_reasons_all_unlocked(&g, &[1.0, 0.0], 5);
		assert!(!hits.is_empty(), "reason search returns hits");
		assert_eq!(hits[0].reason_id, "r_x", "closest reason ranks first");
		assert!(search_reasons_all_unlocked(&GraphGnn::new(), &[1.0, 0.0], 5).is_empty());
		assert!(search_reasons_all_unlocked(&g, &[], 5).is_empty());
	}

	#[test]
	fn find_entity_resolves_through_the_ref_indirection_path() {
		use crate::base_types::{Entity, EntityRef, Kern};
		// "alias" exists only as a ref in ka pointing at "real" in kb, so lookup
		// must miss the direct paths and resolve via kern.refs -> ref_kern.entities.
		let mut g = GraphGnn::new();
		let mut kb = Kern::new("kb", "");
		kb.entities.insert(
			"real".into(),
			Entity {
				id: "real".into(),
				..Default::default()
			},
		);
		let mut ka = Kern::new("ka", "");
		ka.refs.insert(
			"alias".into(),
			EntityRef {
				kern_id: "kb".into(),
				entity_id: "real".into(),
			},
		);
		g.kerns.insert("kb".into(), kb);
		g.kerns.insert("ka".into(), ka);

		let (ent, kern_id) = find_entity(&g, "alias").expect("resolved via ref path");
		assert_eq!(ent.id, "real", "ref resolves to the target entity");
		assert_eq!(
			kern_id, "kb",
			"returns the entity's home kern, not the ref's"
		);
		assert!(find_entity(&g, "nope").is_none());
	}

	#[test]
	fn find_entity_by_prefix_resolves_a_unique_prefix() {
		use crate::base_types::{Entity, Kern};
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert(
			"abc123def".into(),
			Entity {
				id: "abc123def".into(),
				..Default::default()
			},
		);
		g.kerns.insert("kx".into(), k);

		let (hit, kern_id) = find_entity_by_prefix(&g, "abc12").expect("prefix resolves");
		assert_eq!(hit.id, "abc123def");
		assert_eq!(kern_id, "kx");
		assert!(find_entity_by_prefix(&g, "abc123def").is_some());
		assert!(find_entity_by_prefix(&g, "zzz").is_none());
	}

	#[test]
	fn unfiltered_equals_filtered_with_always_true() {
		let g = populated();
		let q = vec![0.5, 0.5, 0.2];
		let plain: std::collections::HashSet<String> = search_all_unlocked(&g, &q, 10)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		let filt: std::collections::HashSet<String> = search_all_filtered(&g, &q, 10, &|_| true)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert_eq!(plain, filt, "always-true filter == unfiltered search");
	}
}
