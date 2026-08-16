//! Tests extracted from lexical.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{Entity, Kern};

	#[test]
	fn stem_strips_known_suffixes_and_guards_short_words() {
		assert_eq!(stem("running"), "runn", "`ing` stripped");
		assert_eq!(stem("cats"), "cat", "`s` stripped");
		assert_eq!(stem("happily"), "happi", "`ly` stripped");
		assert_eq!(stem("bus"), "bus");
		assert_eq!(stem("the"), "the", "no matching suffix");
	}

	#[test]
	fn tokenize_splits_lowercases_and_stems() {
		assert_eq!(tokenize("Running, the Cats!"), vec!["runn", "the", "cat"]);
		assert!(
			tokenize("   ,.!").is_empty(),
			"punctuation-only yields no tokens"
		);
	}

	#[test]
	fn set_bm25_params_changes_query_scores_and_clamps_invalid() {
		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.insert("short", "alpha beta");
		idx.insert("long", "alpha alpha alpha gamma delta epsilon");

		let base: Vec<f32> = idx
			.search("alpha", 10)
			.into_iter()
			.map(|h| h.score)
			.collect();
		idx.set_bm25_params(2.5, 0.0);
		let tuned: Vec<f32> = idx
			.search("alpha", 10)
			.into_iter()
			.map(|h| h.score)
			.collect();
		assert_ne!(
			base, tuned,
			"new k1/b change BM25 scores without re-indexing"
		);

		idx.set_bm25_params(-5.0, 9.0);
		let hits = idx.search("alpha", 10);
		assert!(
			!hits.is_empty() && hits.iter().all(|h| h.score.is_finite()),
			"clamped params keep scores finite: {hits:?}"
		);
		idx.set_bm25_params(f32::NAN, f32::NAN);
		assert!(
			idx.search("alpha", 10).iter().all(|h| h.score.is_finite()),
			"NaN ignored, not applied"
		);
	}

	#[test]
	fn search_ranks_by_bm25_and_excludes_nonmatching_docs() {
		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.insert("d1", "the quick brown fox");
		idx.insert("d2", "lazy dog programming");
		idx.insert("d3", "quick quick fox");

		let hits = idx.search("quick fox", 10);
		assert_eq!(hits.len(), 2, "only docs containing a query term score");
		assert_eq!(hits[0].entity_id, "d3", "higher term frequency ranks first");
		assert_eq!(hits[1].entity_id, "d1");
		assert!(
			!hits.iter().any(|h| h.entity_id == "d2"),
			"d2 shares no terms"
		);
	}

	#[test]
	fn search_filtered_drops_nonmatching_before_truncation() {
		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.insert("drop_a", "rust rust rust");
		idx.insert("drop_b", "rust rust");
		idx.insert("keep_1", "rust");
		idx.insert("keep_2", "rust ownership");
		idx.insert("drop_c", "rust borrow");

		let top1 = idx.search("rust", 1);
		assert!(
			top1[0].entity_id.starts_with("drop_"),
			"unfiltered top-1: {}",
			top1[0].entity_id
		);

		let keep = |id: &str| id.starts_with("keep_");
		let f = idx.search_filtered("rust", 1, &keep);
		assert_eq!(f.len(), 1, "still a full k=1 after filtering");
		assert!(
			f[0].entity_id.starts_with("keep_"),
			"only matching docs survive: {}",
			f[0].entity_id
		);

		let want: std::collections::HashSet<String> =
			["keep_1", "keep_2"].iter().map(|s| s.to_string()).collect();
		let got: std::collections::HashSet<String> = idx
			.search_filtered("rust", 10, &keep)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert_eq!(got, want, "filtered to all matches");

		assert_eq!(
			idx.search("rust", 10).len(),
			5,
			"unfiltered returns all 5 docs"
		);
	}

	#[test]
	fn search_empty_query_or_zero_k_is_empty() {
		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.insert("d1", "hello world");
		assert!(idx.search("", 10).is_empty(), "empty query -> no hits");
		assert!(idx.search("hello", 0).is_empty(), "k=0 -> no hits");
		assert!(
			idx.search("absent", 10).is_empty(),
			"unindexed term -> no hits"
		);
	}

	#[test]
	fn insert_is_an_idempotent_upsert() {
		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.insert("d1", "alpha beta");
		idx.insert("d1", "alpha beta");
		assert_eq!(idx.doc_count(), 1, "re-inserting an id keeps one document");
		idx.insert("d1", "gamma");
		assert!(
			idx.search("alpha", 10).is_empty(),
			"stale terms removed on upsert"
		);
		assert_eq!(idx.search("gamma", 10).len(), 1);
	}

	#[test]
	fn remove_drops_the_document() {
		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.insert("d1", "alpha");
		idx.insert("d2", "alpha");
		idx.remove("d1");
		assert_eq!(idx.doc_count(), 1);
		let hits = idx.search("alpha", 10);
		assert_eq!(hits.len(), 1);
		assert_eq!(hits[0].entity_id, "d2");
	}

	#[test]
	fn rebuild_from_graph_indexes_every_nonempty_entity() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert(
			"e1".into(),
			Entity {
				id: "e1".into(),
				statements: vec!["quick brown fox".into()],
				..Default::default()
			},
		);
		k.entities.insert(
			"e2".into(),
			Entity {
				id: "e2".into(),
				statements: vec!["lazy dog".into()],
				..Default::default()
			},
		);
		k.entities.insert(
			"e3".into(),
			Entity {
				id: "e3".into(),
				..Default::default()
			},
		);
		g.kerns.insert("k".into(), k);

		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.rebuild_from_graph(&g);

		assert_eq!(
			idx.doc_count(),
			2,
			"only the two non-empty entities are indexed"
		);
		let hits = idx.search("fox", 10);
		assert_eq!(hits.len(), 1);
		assert_eq!(hits[0].entity_id, "e1");
	}

	// A rebuild is what every restart and every `kern compact` runs. If it dropped
	// back to `statements` alone, the alternate wording indexed at dedup time would
	// survive exactly until the next reload and nothing would say so.
	#[test]
	fn a_rebuild_keeps_the_alternate_wording_a_dedup_merged_on() {
		use crate::reason::add_reason;
		use base::base_types::Reason;

		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert(
			"e1".into(),
			Entity {
				id: "e1".into(),
				statements: vec!["quick brown fox".into()],
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				id: "r1".into(),
				from: "e1".into(),
				kind: ReasonKind::Rephrase,
				text: "a swift auburn vulpine".into(),
				..Default::default()
			},
		);
		g.kerns.insert("k".into(), k);

		let idx = LexicalIndex::new_in_ram(1.2, 0.75);
		idx.rebuild_from_graph(&g);

		assert_eq!(idx.doc_count(), 1, "still one document for one entity");
		let hits = idx.search("vulpine", 10);
		assert_eq!(
			hits.len(),
			1,
			"the merged-away wording survives the rebuild: {hits:?}"
		);
		assert_eq!(hits[0].entity_id, "e1");
		assert_eq!(
			idx.search("fox", 10).len(),
			1,
			"and the primary wording is not displaced by it"
		);
	}
}
