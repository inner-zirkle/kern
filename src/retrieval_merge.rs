//! Merge seed hits with walk-expanded results into one candidate list,
//! softmax-normalising scores so direct hits and chain-reached entities
//! compete on one scale without a raw-score cliff between them.

use crate::graph::GraphGnn;
use crate::math::OnlineSoftmax;
use crate::retrieval::expand::{find_entity_ref_in_graph, ScoredRef};
use crate::search::EntityHit;
use std::collections::HashMap;

// Log-sum-exp score pool: an entity in both sources earns +ln(count). Result is a magnitude, not a probability — may exceed 1.0.
pub fn merge<'a>(
	g: &'a GraphGnn,
	seeds: &[EntityHit],
	beam: Vec<ScoredRef<'a>>,
) -> Vec<ScoredRef<'a>> {
	let mut scores: HashMap<&str, OnlineSoftmax> = HashMap::new();
	let mut thoughts: HashMap<&str, ScoredRef<'a>> = HashMap::new();

	for st in beam {
		scores.entry(&st.entity.id).or_default().update(st.score);
		thoughts.entry(&st.entity.id).or_insert(st);
	}

	for s in seeds {
		if let Some(t) = thoughts.get(s.entity_id.as_str()) {
			scores.entry(&t.entity.id).or_default().update(s.score);
		} else if let Some(t) = find_entity_ref_in_graph(g, &s.entity_id) {
			scores.entry(&t.id).or_default().update(s.score);
			thoughts.insert(
				&t.id,
				ScoredRef {
					entity: t,
					score: s.score,
				},
			);
		}
	}

	let mut results: Vec<ScoredRef<'a>> = thoughts
		.into_iter()
		.filter_map(|(id, mut st)| {
			let merged = scores.get(id)?.finalize();
			st.score = merged;
			Some(st)
		})
		.collect();

	// Score desc, id asc — the id tie-break is required for determinism (HashMap order varies per process).
	results.sort_by(|a, b| crate::util::cmp_rank(a.score, &a.entity.id, b.score, &b.entity.id));
	results
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base_types::Kern;

	use crate::base_types::Entity;
	use crate::test_support::entity as ent;
	fn hit(id: &str, score: f64) -> EntityHit {
		EntityHit {
			entity_id: id.into(),
			score,
		}
	}
	fn scored(entity: &Entity, score: f64) -> ScoredRef<'_> {
		ScoredRef { entity, score }
	}
	fn find<'a, 'g>(rs: &'a [ScoredRef<'g>], id: &str) -> Option<&'a ScoredRef<'g>> {
		rs.iter().find(|s| s.entity.id == id)
	}

	#[test]
	fn entity_seen_in_both_sources_outranks_one_seen_once() {
		let g = GraphGnn::new();
		let (ea, eb) = (ent("a"), ent("b"));
		let beam = vec![scored(&ea, 0.5), scored(&eb, 0.5)];
		let seeds = [hit("a", 0.5)];
		let out = merge(&g, &seeds, beam);

		let a = find(&out, "a").expect("a present");
		let b = find(&out, "b").expect("b present");
		assert!(
			a.score > b.score,
			"corroborated a ({}) > lone b ({})",
			a.score,
			b.score
		);
		assert!((a.score - (0.5 + std::f64::consts::LN_2)).abs() < 1e-9);
		assert!((b.score - 0.5).abs() < 1e-9);
		assert_eq!(out[0].entity.id, "a", "higher score sorts first");
	}

	#[test]
	fn seed_absent_from_graph_and_beam_is_silently_skipped() {
		let g = GraphGnn::new();
		let eb = ent("b");
		let beam = vec![scored(&eb, 0.5)];
		let seeds = [hit("ghost", 0.9)];
		let out = merge(&g, &seeds, beam);

		assert!(find(&out, "ghost").is_none(), "unresolvable seed dropped");
		assert_eq!(out.len(), 1, "only the beam entity survives");
		assert_eq!(out[0].entity.id, "b");
	}

	#[test]
	fn seed_only_entity_is_pulled_from_the_graph() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("c".into(), ent("c"));
		g.kerns.insert("kx".into(), k);

		let out = merge(&g, &[hit("c", 0.7)], Vec::new());
		let c = find(&out, "c").expect("seed resolved from graph");
		assert!((c.score - 0.7).abs() < 1e-9, "single observation unchanged");
	}
}
