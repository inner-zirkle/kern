//! Tests extracted from reason.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{Entity, EntityKind, Kern};

	use test_support::{edge, entity_vec as ent};

	#[test]
	fn superseded_ancestors_walks_the_supersedes_chain_backward() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		for id in ["newest", "mid", "old"] {
			g.get_mut(&root).unwrap().entities.insert(
				id.into(),
				Entity {
					id: id.into(),
					..Default::default()
				},
			);
			g.index_entity(id, &root);
		}
		let k = g.get_mut(&root).unwrap();
		add_reason(
			k,
			Reason {
				id: "s1".into(),
				from: "newest".into(),
				to: "mid".into(),
				kind: ReasonKind::Supersedes,
				..Default::default()
			},
		);
		add_reason(
			k,
			Reason {
				id: "s2".into(),
				from: "mid".into(),
				to: "old".into(),
				kind: ReasonKind::Supersedes,
				..Default::default()
			},
		);

		let mut anc = superseded_ancestors(&g, "newest");
		anc.sort();
		assert_eq!(anc, vec!["mid".to_string(), "old".to_string()]);
		assert!(superseded_ancestors(&g, "old").is_empty());
	}

	#[test]
	fn add_reason_is_idempotent_on_adjacency() {
		let mut k = Kern::new("k", "");
		add_reason(&mut k, edge("a", "b"));
		add_reason(&mut k, edge("a", "b"));
		add_reason(&mut k, edge("a", "b"));

		assert_eq!(k.reasons.len(), 1, "one reason in the map");
		assert_eq!(
			k.by_from.get("a").map(|v| v.len()),
			Some(1),
			"no dup in by_from"
		);
		assert_eq!(
			k.by_to.get("b").map(|v| v.len()),
			Some(1),
			"no dup in by_to"
		);
		assert_eq!(collect_reason_ids(&k, "a"), vec!["a->b".to_string()]);
	}

	#[test]
	fn remove_after_reobserve_fully_clears_adjacency() {
		let mut k = Kern::new("k", "");
		add_reason(&mut k, edge("a", "b"));
		add_reason(&mut k, edge("a", "b"));
		remove_reason(&mut k, "a->b");

		assert!(k.reasons.is_empty(), "reason removed from map");
		assert!(
			k.by_from.get("a").map(|v| v.is_empty()).unwrap_or(true),
			"no stale id left in by_from"
		);
		assert!(
			collect_reason_ids(&k, "a").is_empty(),
			"no dangling edge id"
		);
	}

	fn move_fixture() -> GraphGnn {
		let mut g = GraphGnn::new();
		let mut src = Kern::new("src", "");
		src.entities.insert("E".into(), ent("E", vec![]));
		src.entities.insert("X".into(), ent("X", vec![]));
		add_reason(&mut src, edge("E", "X"));
		add_reason(&mut src, edge("E", "E"));
		add_reason(&mut src, edge("Y", "E"));
		g.kerns.insert("src".into(), src);
		g
	}

	#[test]
	fn move_entity_relocates_outgoing_and_stamps_cross_kern_targets() {
		let mut g = move_fixture();
		g.kerns.insert("dst".into(), Kern::new("dst", ""));

		assert_eq!(move_entity(&mut g, "src", "dst", "E"), Ok(()));

		let dst = g.kerns.get("dst").unwrap();
		let src = g.kerns.get("src").unwrap();
		assert!(dst.entities.contains_key("E"), "entity moved to dst");
		assert!(!src.entities.contains_key("E"), "entity gone from src");

		assert_eq!(
			dst.reasons.get("E->X").map(|r| r.to_kern_id.as_str()),
			Some("src"),
			"outgoing edge to an entity left behind is stamped back to src"
		);
		assert!(
			!src.reasons.contains_key("E->X"),
			"outgoing detached from src maps"
		);
		assert!(
			src.by_from.get("E").map(|v| v.is_empty()).unwrap_or(true),
			"src by_from[E] cleared"
		);
		assert_eq!(
			dst.reasons.get("E->E").map(|r| r.to_kern_id.as_str()),
			Some(""),
			"self-loop travels with the entity, unstamped"
		);

		assert_eq!(
			src.reasons.get("Y->E").map(|r| r.to_kern_id.as_str()),
			Some("dst"),
			"incoming edge stays in src, restamped at dst"
		);
		assert!(
			!dst.reasons.contains_key("Y->E"),
			"incoming reason not moved"
		);

		assert_eq!(g.kern_of_entity("E"), Some("dst"), "entity index follows");
	}

	// Regression: the destination check once ran AFTER the entity and its outgoing
	// reasons had already been ripped out of src, so a bad `to_kern_id` deleted them.
	#[test]
	fn move_entity_to_missing_destination_leaves_source_untouched() {
		let mut g = move_fixture();
		let before = g.kerns.get("src").unwrap().clone();

		assert_eq!(
			move_entity(&mut g, "src", "ghost_kern", "E"),
			Err(MoveError::KernNotFound("ghost_kern".into()))
		);

		let src = g.kerns.get("src").unwrap();
		assert!(src.entities.contains_key("E"), "entity NOT lost");
		assert_eq!(src.entities.len(), before.entities.len());
		assert_eq!(
			src.reasons.len(),
			before.reasons.len(),
			"no reason removed on a rejected move"
		);
		for (id, r) in &before.reasons {
			let now = src.reasons.get(id).expect("reason survived");
			assert_eq!(
				now.to_kern_id.as_str(),
				r.to_kern_id.as_str(),
				"{id} not restamped by a rejected move"
			);
		}
		assert_eq!(src.by_from, before.by_from, "by_from adjacency untouched");
		assert_eq!(src.by_to, before.by_to, "by_to adjacency untouched");
	}

	#[test]
	fn move_entity_rejects_missing_source_kern_and_missing_entity() {
		let mut g = move_fixture();
		g.kerns.insert("dst".into(), Kern::new("dst", ""));

		assert_eq!(
			move_entity(&mut g, "ghost", "dst", "E"),
			Err(MoveError::KernNotFound("ghost".into()))
		);
		assert_eq!(
			move_entity(&mut g, "src", "dst", "ghost_entity"),
			Err(MoveError::EntityNotFound {
				kern: "src".into(),
				entity: "ghost_entity".into(),
			})
		);
		assert!(g.kerns.get("dst").unwrap().entities.is_empty());
		assert!(g.kerns.get("src").unwrap().entities.contains_key("E"));
	}

	#[test]
	fn move_entity_same_kern_is_a_validated_noop() {
		let mut g = move_fixture();
		let before = g.kerns.get("src").unwrap().clone();

		assert_eq!(move_entity(&mut g, "src", "src", "E"), Ok(()));

		let src = g.kerns.get("src").unwrap();
		assert_eq!(src.entities.len(), before.entities.len());
		assert_eq!(src.reasons.len(), before.reasons.len());
		assert_eq!(src.by_from, before.by_from, "self-move changes nothing");
		assert_eq!(src.by_to, before.by_to, "self-move changes nothing");
	}

	#[test]
	fn remove_entity_cascades_through_reasons_and_hnsw_indices() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert("a".into(), ent("a", vec![1.0, 0.0]));
		k.entities.insert("b".into(), ent("b", vec![0.0, 1.0]));
		let mut e1 = edge("a", "b");
		e1.vector = vec![0.5, 0.5].into();
		let mut e2 = edge("b", "a");
		e2.vector = vec![0.4, 0.6].into();
		add_reason(&mut k, e1);
		add_reason(&mut k, e2);
		g.kerns.insert("k".into(), k);
		g.rebuild_index();
		assert_eq!(g.entity_idx.len(), 2, "two entities indexed");
		assert_eq!(g.reason_idx.len(), 2, "two reasons indexed");

		remove_entity(&mut g, "k", "a", false);

		let k = g.kerns.get("k").unwrap();
		assert!(!k.entities.contains_key("a"), "entity removed from map");
		assert!(!k.by_from.contains_key("a"), "by_from[a] purged");
		assert!(!k.by_to.contains_key("a"), "by_to[a] purged");
		assert!(
			k.reasons.is_empty(),
			"both incident reasons removed (a->b and b->a)"
		);
		assert!(
			collect_reason_ids(k, "b").is_empty(),
			"b left with no dangling edges"
		);
		assert_eq!(
			g.entity_idx.len(),
			1,
			"entity a purged from entity_idx, b remains"
		);
		assert_eq!(g.reason_idx.len(), 0, "both reasons purged from reason_idx");
	}

	#[test]
	fn remove_entity_fact_is_immune() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		let fact = Entity {
			id: "f".into(),
			kind: EntityKind::Fact,
			..Default::default()
		};
		k.entities.insert("f".into(), fact);
		g.kerns.insert("k".into(), k);

		remove_entity(&mut g, "k", "f", false);
		assert!(
			g.kerns.get("k").unwrap().entities.contains_key("f"),
			"facts are immune to removal"
		);

		// The one bypass (ROADMAP item 19). Without it here the outer guard could
		// be lifted and the removal would still silently not happen.
		remove_entity(&mut g, "k", "f", true);
		assert!(
			!g.kerns.get("k").unwrap().entities.contains_key("f"),
			"force punches through local fact-immunity"
		);
	}
}
