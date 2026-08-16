//! Tests extracted from search.rs
#![allow(unused)]
use super::*;

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
		use base::base_types::{Entity, Kern};
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
		use base::base_types::{Entity, EntityRef, Kern};
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
		use base::base_types::{Entity, Kern};
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
