//! Tests extracted from tick_tasks.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{Entity, Kern};
	use graph::graph::GraphGnn;
	use parking_lot::RwLock;
	use std::sync::Arc;

	#[test]
	fn do_seed_questions_adds_question_edges_for_the_entity() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut e = Entity {
			id: "e1".into(),
			..Default::default()
		};
		e.set_text("the spawn gate shipped today".into());
		g.kerns
			.get_mut(&root)
			.unwrap()
			.entities
			.insert("e1".into(), e);
		g.rebuild_index();
		let g = Arc::new(RwLock::new(g));

		let llm: LlmFunc =
			Arc::new(|_p: &str| "What shipped today?\nWhen did the gate ship?".to_string());
		let q = Queue::new(16);
		do_seed_questions(&q, &g, "e1", Some(&llm));

		let gg = g.read();
		let qs: Vec<_> = gg
			.kerns
			.get(&root)
			.unwrap()
			.reasons
			.values()
			.filter(|r| r.kind == ReasonKind::Question && r.from == "e1" && r.to.is_empty())
			.collect();
		assert_eq!(qs.len(), 2, "one dangling Question edge per LLM line");
		drop(gg);

		let mut rx = q.take_receiver().unwrap();
		let mut persists = Vec::new();
		while let Ok(t) = rx.try_recv() {
			if matches!(t.kind, TaskKind::Persist) {
				persists.push(t.kern_id.clone());
			}
		}
		assert_eq!(
			persists,
			vec![root.clone()],
			"seeded Question edges are followed by a root Persist — without it they lived only in RAM until an unrelated flush"
		);
	}

	fn graph_with_rephrase(
		old_text: &str,
		new_text: &str,
	) -> (Arc<RwLock<GraphGnn>>, String, String) {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut old = Entity {
			id: "old".into(),
			kind: base::base_types::EntityKind::Claim,
			vector: vec![1.0, 0.0].into(),
			..Default::default()
		};
		old.set_text(old_text.into());
		old.dirty = false;
		g.get_mut(&root).unwrap().entities.insert("old".into(), old);
		g.index_entity("old", &root);
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		let rid = reason_id("old", "", ReasonKind::Rephrase, new_text);
		add_reason(
			g.get_mut(&root).unwrap(),
			Reason {
				id: rid.clone(),
				from: "old".into(),
				to: String::new(),
				kind: ReasonKind::Rephrase,
				text: new_text.into(),
				..Default::default()
			},
		);
		(Arc::new(RwLock::new(g)), root, rid)
	}

	#[test]
	fn classify_contradiction_supersedes_on_update_verdict() {
		let (g, root, rid) = graph_with_rephrase("the deadline is March", "the deadline is April");
		let llm: LlmFunc = Arc::new(|_p: &str| "CONTRADICTION".to_string());
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.9, 0.1]));
		let q = Queue::new(16);
		do_classify_contradiction(&q, &g, &root, &rid, Some(&llm), Some(&embed));

		let gg = g.read();
		let kern = gg.kerns.get(&root).unwrap();
		let old = kern.entities.get("old").unwrap();
		assert!(
			old.is_superseded(),
			"old is superseded on a CONTRADICTION verdict"
		);
		assert!(old.invalidated_at.is_some(), "old stamped invalidated");
		let new_id = util::content_hash("the deadline is April");
		assert!(
			kern.entities.contains_key(&new_id),
			"new revision materialized"
		);
		assert_eq!(old.superseded_by, new_id);
		assert!(
			!kern.reasons.contains_key(&rid),
			"the Rephrase edge is retired once it becomes a Supersedes edge"
		);
		assert!(
			kern
				.reasons
				.values()
				.any(|r| r.kind == ReasonKind::Supersedes),
			"a Supersedes edge now links the revisions"
		);
	}

	#[test]
	fn classify_contradiction_keeps_rephrase_on_related_verdict() {
		let (g, root, rid) = graph_with_rephrase("cats are mammals", "cats are feline mammals");
		let llm: LlmFunc = Arc::new(|_p: &str| "RELATED".to_string());
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.9, 0.1]));
		let q = Queue::new(16);
		do_classify_contradiction(&q, &g, &root, &rid, Some(&llm), Some(&embed));

		let gg = g.read();
		let kern = gg.kerns.get(&root).unwrap();
		assert!(
			!kern.entities.get("old").unwrap().is_superseded(),
			"a RELATED verdict leaves the stored claim active"
		);
		assert!(
			kern.reasons.contains_key(&rid),
			"the Rephrase edge stands unchanged on RELATED"
		);
	}

	#[test]
	fn classify_contradiction_is_a_noop_without_llm() {
		let (g, root, rid) = graph_with_rephrase("a", "b");
		let q = Queue::new(16);
		do_classify_contradiction(&q, &g, &root, &rid, None, None);
		let gg = g.read();
		let kern = gg.kerns.get(&root).unwrap();
		assert!(!kern.entities.get("old").unwrap().is_superseded());
		assert!(kern.reasons.contains_key(&rid), "rephrase edge preserved");
	}

	#[test]
	fn do_seed_questions_is_a_noop_without_llm_or_entity() {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let q = Queue::new(16);
		do_seed_questions(&q, &g, "e1", None);
		let llm: LlmFunc = Arc::new(|_p: &str| "Q?".to_string());
		do_seed_questions(&q, &g, "missing", Some(&llm));
		let gg = g.read();
		let root = gg.root.id.clone();
		assert!(
			gg.kerns.get(&root).unwrap().reasons.is_empty(),
			"no edges minted"
		);
	}

	#[test]
	fn do_commit_access_stamps_the_live_entities_from_the_id_list() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		g.index_entity("a", &kid);
		let epoch_before = g.mutation_epoch();
		let g = Arc::new(RwLock::new(g));

		do_commit_access(&g, "a", &HeatConfig::default());

		let gg = g.read();
		let live = gg.kerns.get(&kid).unwrap().entities.get("a").unwrap();
		assert!(
			live.accessed_at.is_some(),
			"the deferred stamp reached the live entity"
		);
		assert_eq!(
			live.access_count.value(),
			1,
			"live access counter bumped by the tick"
		);
		assert_eq!(
			gg.mutation_epoch(),
			epoch_before,
			"the access stamp must not invalidate the query cache"
		);
	}

	#[test]
	fn do_reembed_clears_dirty_and_sets_vector() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		let mut e = Entity {
			id: "e1".into(),
			dirty: true,
			..Default::default()
		};
		e.set_text("hello world".into());
		kern.entities.insert(e.id.clone(), e);
		g.kerns.insert(kid.clone(), kern);
		let g = Arc::new(RwLock::new(g));
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.1, 0.2, 0.3]));
		do_reembed(&g, &kid, Some(&embed));
		let g = g.read();
		let e = g.kerns.get(&kid).unwrap().entities.get("e1").unwrap();
		assert!(!e.dirty, "dirty must be cleared after reembed");
		assert_eq!(e.vector[..], [0.1, 0.2, 0.3]);
	}

	#[test]
	fn do_reembed_shares_vector_allocation_with_gnn_vector() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		let mut e = Entity {
			id: "e1".into(),
			dirty: true,
			..Default::default()
		};
		e.set_text("hello world".into());
		kern.entities.insert(e.id.clone(), e);
		g.kerns.insert(kid.clone(), kern);
		let g = Arc::new(RwLock::new(g));
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.1, 0.2, 0.3]));
		do_reembed(&g, &kid, Some(&embed));
		let g = g.read();
		let e = g.kerns.get(&kid).unwrap().entities.get("e1").unwrap();
		assert!(
			std::sync::Arc::ptr_eq(&e.vector, &e.gnn_vector),
			"reembed must share the Arc between vector and gnn_vector, not allocate twice"
		);
	}

	#[test]
	fn do_reembed_recomputes_dirty_reason_as_endpoint_mean() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			},
		);
		kern.entities.insert(
			"b".into(),
			Entity {
				id: "b".into(),
				vector: vec![0.0, 1.0].into(),
				..Default::default()
			},
		);
		add_reason(
			&mut kern,
			Reason {
				id: "a->b".into(),
				from: "a".into(),
				to: "b".into(),
				dirty: true,
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		let g = Arc::new(RwLock::new(g));

		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![9.0, 9.0]));
		do_reembed(&g, &kid, Some(&embed));

		let g = g.read();
		let r = g.kerns.get(&kid).unwrap().reasons.get("a->b").unwrap();
		assert!(!r.dirty, "dirty reason cleared once recomputed");
		assert_eq!(
			r.vector,
			vec![0.5, 0.5].into(),
			"reason vector is the mean of endpoint vectors"
		);
	}

	#[test]
	fn do_resolve_links_question_to_nearest_entity_above_threshold() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"target".into(),
			Entity {
				id: "target".into(),
				vector: vec![1.0, 0.0, 0.0].into(),
				..Default::default()
			},
		);
		kern.entities.insert(
			"asker".into(),
			Entity {
				id: "asker".into(),
				vector: vec![0.0, 1.0, 0.0].into(),
				..Default::default()
			},
		);
		add_reason(
			&mut kern,
			Reason {
				id: "q1".into(),
				from: "asker".into(),
				to: String::new(),
				kind: ReasonKind::Question,
				vector: vec![1.0, 0.0, 0.0].into(),
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		g.rebuild_index();
		let g = Arc::new(RwLock::new(g));

		let q = Queue::new(16);
		do_resolve(&q, &g, &kid, "q1");

		let g = g.read();
		let r = g.kerns.get(&kid).unwrap().reasons.get("q1").unwrap();
		assert_eq!(
			r.kind,
			ReasonKind::Similarity,
			"resolved question becomes a Similarity edge"
		);
		assert_eq!(r.to, "target", "linked to the nearest indexed entity");
	}

	#[test]
	fn do_resolve_ignores_non_question_or_already_linked() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"target".into(),
			Entity {
				id: "target".into(),
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			},
		);
		add_reason(
			&mut kern,
			Reason {
				id: "linked".into(),
				from: "x".into(),
				to: "y".into(),
				kind: ReasonKind::Question,
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		g.rebuild_index();
		let g = Arc::new(RwLock::new(g));

		let q = Queue::new(16);
		do_resolve(&q, &g, &kid, "linked");

		let g = g.read();
		let r = g.kerns.get(&kid).unwrap().reasons.get("linked").unwrap();
		assert_eq!(
			r.kind,
			ReasonKind::Question,
			"already-linked question is untouched"
		);
		assert_eq!(r.to, "y", "existing link preserved");
	}

	#[test]
	fn strip_name_prefixes_removes_first_known_label_only() {
		assert_eq!(
			strip_name_prefixes("Theme: rust ownership"),
			"rust ownership"
		);
		assert_eq!(
			strip_name_prefixes("  name:  caching layer  "),
			"caching layer"
		);
		assert_eq!(strip_name_prefixes("Label:x"), "x");
		assert_eq!(strip_name_prefixes("  plain phrase "), "plain phrase");
		assert_eq!(strip_name_prefixes("Theme: Name: nested"), "Name: nested");
	}
}
