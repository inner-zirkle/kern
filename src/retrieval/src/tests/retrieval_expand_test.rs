//! Tests extracted from retrieval_expand.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use graph::reason::add_reason;

	use test_support::entity_vec as ent;
	fn edge(from: &str, to: &str, score: f64) -> Reason {
		Reason {
			id: format!("{from}->{to}"),
			from: from.into(),
			to: to.into(),
			score,
			kind: ReasonKind::Similarity,
			..Default::default()
		}
	}

	#[test]
	fn score_neighbor_pure_content_weight_is_cosine() {
		let neighbor = ent("n", vec![1.0, 0.0]);
		let r = edge("a", "n", 0.5);
		let w = Weights {
			content: 1.0,
			reason: 0.0,
			edge: 0.0,
		};
		let s = score_neighbor(&[1.0, 0.0], &neighbor, &r, w, 0.1, 0.3);
		assert!(
			(s - 1.0).abs() < 1e-9,
			"query aligned with neighbour -> 1.0"
		);
	}

	#[test]
	fn score_neighbor_pure_edge_weight_uses_clamped_reason_score() {
		let neighbor = ent("n", vec![]);
		let r = edge("a", "n", 0.4);
		let w = Weights {
			content: 0.0,
			reason: 0.0,
			edge: 1.0,
		};
		let s = score_neighbor(&[1.0, 0.0], &neighbor, &r, w, 0.1, 0.3);
		assert!(
			(s - 0.4).abs() < 1e-9,
			"edge component is the clamped reason score"
		);
	}

	#[test]
	fn expand_walks_edges_from_seed_and_records_a_chain() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		for id in ["a", "b", "c"] {
			k.entities.insert(id.into(), ent(id, vec![1.0, 0.0]));
		}
		add_reason(&mut k, edge("a", "b", 0.9));
		add_reason(&mut k, edge("b", "c", 0.9));
		g.kerns.insert("kx".into(), k);

		let cfg = RetrievalConfig::default();
		let seeds = [EntityHit {
			entity_id: "a".into(),
			score: 1.0,
		}];
		let w = Weights {
			content: 1.0,
			reason: 0.0,
			edge: 0.0,
		};
		let res = expand(&g, &cfg, &[1.0, 0.0], &seeds, w);

		let ids: HashSet<&str> = res.scored.iter().map(|s| s.entity.id.as_str()).collect();
		assert!(ids.contains("a"), "the seed is scored");
		assert!(
			ids.contains("b"),
			"the 1-hop neighbour is reached via the edge"
		);
		assert!(
			res.chains.iter().any(|c| c.nodes.len() >= 3),
			"a multi-hop chain (entity, reason, entity) is recorded"
		);
	}
	fn linked_pair_graph() -> GraphGnn {
		// a matches the query [1,0] exactly; b is orthogonal, reachable only
		// across the edge. Mirrors the ROADMAP item 86 measurement: b is also a
		// (weak) content hit, so the max-per-entity walk score alone gives the
		// edge no way to move it.
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("a".into(), ent("a", vec![1.0, 0.0]));
		k.entities.insert("b".into(), ent("b", vec![0.0, 1.0]));
		let mut r = edge("a", "b", 0.9);
		r.vector = vec![0.7, 0.7].into();
		add_reason(&mut k, r);
		g.kerns.insert("kx".into(), k);
		g
	}

	const PAIR_WEIGHTS: Weights = Weights {
		content: 0.70,
		reason: 0.15,
		edge: 0.15,
	};

	fn pair_seeds() -> [EntityHit; 2] {
		[
			EntityHit {
				entity_id: "a".into(),
				score: 1.0,
			},
			EntityHit {
				entity_id: "b".into(),
				score: 0.0,
			},
		]
	}

	fn score_of(res: &ExpandResult, id: &str) -> f64 {
		res
			.scored
			.iter()
			.find(|s| s.entity.id == id)
			.unwrap_or_else(|| panic!("{id} missing from scored"))
			.score
	}

	#[test]
	fn an_edge_off_a_strong_seed_lifts_a_neighbour_that_is_already_a_weak_hit() {
		let g = linked_pair_graph();
		let cfg = RetrievalConfig::default();
		let res = expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS);

		let evidence = 0.15 * (0.7 / (0.7f32 * 0.7 + 0.7 * 0.7).sqrt() as f64) + 0.15 * 0.9;
		let b = score_of(&res, "b");
		assert!(
			b > evidence + 1e-6,
			"b must carry credit ON TOP of its walk score, got {b} vs evidence {evidence}"
		);
		let a = score_of(&res, "a");
		assert!(
			a > b,
			"the direct match still outranks the lifted neighbour"
		);
	}

	#[test]
	fn credit_from_a_weaker_voucher_cannot_lift_past_the_voucher() {
		// b pops at its edge-derived walk score and credits a back across the
		// same edge, but a already outranks b — the ceiling annuls the lift, so
		// the direct answer's score is exactly its walk score, not walk + bonus.
		let g = linked_pair_graph();
		let cfg = RetrievalConfig::default();
		let res = expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS);

		let a = score_of(&res, "a");
		assert!(
			(a - 1.0).abs() < 1e-9,
			"credit sourced below the seed must not move it, got {a}"
		);
	}

	#[test]
	fn a_lifted_neighbour_saturates_just_below_its_strongest_voucher() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("c".into(), ent("c", vec![1.0, 0.0]));
		k.entities.insert("n".into(), ent("n", vec![0.6, 0.8]));
		let mut r = edge("c", "n", 1.0);
		r.vector = vec![1.0, 0.0].into();
		add_reason(&mut k, r);
		g.kerns.insert("kx".into(), k);

		let cfg = RetrievalConfig {
			traversal_credit_cap: 1.0,
			..Default::default()
		};
		let seeds = [
			EntityHit {
				entity_id: "c".into(),
				score: 1.0,
			},
			EntityHit {
				entity_id: "n".into(),
				score: 0.6,
			},
		];
		let res = expand(&g, &cfg, &[1.0, 0.0], &seeds, PAIR_WEIGHTS);

		let (c, n) = (score_of(&res, "c"), score_of(&res, "n"));
		assert!(
			n < c,
			"the lifted neighbour stays behind its voucher: n={n} c={c}"
		);
		assert!(
			n > 0.9,
			"but the ceiling, not the cap, is what stopped it: n={n}"
		);
	}

	#[test]
	fn traversal_credit_is_capped() {
		let g = linked_pair_graph();
		let mut cfg = RetrievalConfig {
			traversal_credit_cap: 0.0,
			..Default::default()
		};
		let off = score_of(
			&expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS),
			"b",
		);

		cfg.traversal_credit_cap = 0.01;
		let capped = score_of(
			&expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS),
			"b",
		);

		assert!(
			(capped - (off + 0.01)).abs() < 1e-9,
			"bonus must saturate at the cap: off={off} capped={capped}"
		);
	}

	#[test]
	fn a_strong_seed_no_longer_prunes_the_walk_off_it() {
		// The seed scale (pure query cosine, up to 1.0) and the neighbour scale
		// (0.70*content + 0.15*reason + 0.15*edge, so at most 0.30 for a neighbour
		// the query does not match) are different scales. Thresholding one against
		// the other killed traversal whenever a seed matched well.
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("a".into(), ent("a", vec![1.0, 0.0]));
		// Orthogonal to the query: reachable only across the edge.
		k.entities.insert("b".into(), ent("b", vec![0.0, 1.0]));
		let mut r = edge("a", "b", 0.9);
		r.vector = vec![0.7, 0.7].into();
		add_reason(&mut k, r);
		g.kerns.insert("kx".into(), k);

		let cfg = RetrievalConfig::default();
		let seeds = [EntityHit {
			entity_id: "a".into(),
			score: 1.0,
		}];
		let w = Weights {
			content: 0.70,
			reason: 0.15,
			edge: 0.15,
		};
		let res = expand(&g, &cfg, &[1.0, 0.0], &seeds, w);

		let ids: HashSet<&str> = res.scored.iter().map(|s| s.entity.id.as_str()).collect();
		assert!(
			ids.contains("b"),
			"a neighbour off a perfectly-matching seed must still be walked; \
			 got {ids:?}"
		);
		assert!(
			!res.chains.is_empty(),
			"and the walk must be recorded as a chain"
		);
	}
}
