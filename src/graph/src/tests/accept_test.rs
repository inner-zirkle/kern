//! Tests extracted from accept.rs
#![allow(unused)]
use super::*;

pub(crate) static SUPERSEDE_CHAIN_TEST_MUX: std::sync::Mutex<()> = std::sync::Mutex::new(());
mod tests {
	use super::*;
	use crate::graph::GraphGnn;

	fn ent(id: &str, vector: Vec<f32>) -> Entity {
		Entity {
			id: id.into(),
			vector: vector.into(),
			statements: vec!["x".into()],
			..Default::default()
		}
	}

	#[test]
	fn unnamed_child_reused_when_evicted_by_load_cap() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			store_core::Store::open(&g.data_dir).unwrap(),
		));
		g.set_max_loaded_kerns(1);
		let root = g.root.id.clone();

		let first = get_or_spawn_unnamed_child(&mut g, &root);
		for _ in 0..20 {
			let id = get_or_spawn_unnamed_child(&mut g, &root);
			assert_eq!(id, first, "must reuse the evicted unnamed child");
		}
		assert_eq!(g.count(), 2, "no runaway kern creation under the cap");
	}

	#[test]
	fn generic_child_reused_when_evicted_by_load_cap() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			store_core::Store::open(&g.data_dir).unwrap(),
		));
		g.set_max_loaded_kerns(1);
		let root = g.root.id.clone();

		let first = get_or_spawn_generic_child(&mut g, &root);
		for _ in 0..20 {
			let id = get_or_spawn_generic_child(&mut g, &root);
			assert_eq!(id, first, "must reuse the evicted generic child");
		}
		assert_eq!(
			g.count(),
			2,
			"exactly one generic child created, no runaway"
		);
	}

	#[test]
	fn unnamed_child_not_duplicated_when_non_root_parent_evicts() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			store_core::Store::open(&g.data_dir).unwrap(),
		));
		g.set_max_loaded_kerns(1);
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();

		let parent = {
			let p = Kern::new_named_child(&root, &root_net, "parent-graviton", vec![1.0, 0.0]);
			let pid = p.id.clone();
			g.register(p);
			if let Some(r) = g.get_mut(&root) {
				if !r.children.contains(&pid) {
					r.children.push(pid.clone());
				}
			}
			pid
		};

		let first = get_or_spawn_unnamed_child(&mut g, &parent);
		for _ in 0..20 {
			let id = get_or_spawn_unnamed_child(&mut g, &parent);
			assert_eq!(
				id, first,
				"reuse the unnamed child even when the non-root parent evicted"
			);
		}
		assert_eq!(
			g.count(),
			3,
			"no runaway: root + parent + one unnamed child"
		);
	}

	// ROADMAP item 83: the cluster path uses `spawn_unnamed_child` (always a
	// distinct child), unlike `get_or_spawn_unnamed_child` (reuses). Under a cap,
	// `register` inside `spawn_unnamed_child` can evict the parent before its
	// `children` list gains the new id. This pins that the parent's persisted
	// `children` survives the eviction — no re-spawn loop, no fragmentation.
	#[test]
	fn spawn_unnamed_child_under_cap_keeps_the_child_in_parent_children() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		g.set_store(std::sync::Arc::new(
			store_core::Store::open(&g.data_dir).unwrap(),
		));
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();
		// cap = 2 so registering a child under a non-root parent forces eviction
		g.set_max_loaded_kerns(2);
		let parent = {
			let p = Kern::new_named_child(&root, &root_net, "parent-graviton", vec![1.0, 0.0]);
			let pid = p.id.clone();
			g.register(p);
			if let Some(r) = g.get_mut(&root) {
				if !r.children.contains(&pid) {
					r.children.push(pid.clone());
				}
			}
			pid
		};
		let child = spawn_unnamed_child(&mut g, &parent);
		// reload the parent from the store — the eviction inside `register` may
		// have unloaded it, so `loaded` alone could miss the persisted children.
		let reloaded_children = g
			.get(&parent)
			.map(|k| k.children.clone())
			.unwrap_or_default();
		assert!(
			reloaded_children.contains(&child),
			"the new child must be in the parent's persisted children after eviction: got {reloaded_children:?}"
		);
		assert_eq!(
			g.count(),
			3,
			"root + parent + one child, no re-spawn runaway"
		);
	}

	// ROADMAP item 60: superseding an entity that carries a deferred Rephrase
	// candidate re-points the candidate to the new active entity and queues it
	// for re-classification, so it is not orphaned by `do_classify_contradiction`'s
	// `old.is_superseded()` early return.
	#[test]
	fn supersede_repoints_a_deferred_rephrase_to_the_new_entity() {
		use crate::reason::add_reason;
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let mut old = Entity {
			id: "old".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			..Default::default()
		};
		old.statements = vec!["old claim".into()];
		g.get_mut(&kid).unwrap().entities.insert("old".into(), old);
		g.get_mut(&kid)
			.unwrap()
			.source_index
			.insert("ext1".into(), "old".into());
		g.index_entity("old", &kid);
		// a deferred contradiction candidate: Rephrase on `old`, `to` empty
		let rid = reason_id("old", "", ReasonKind::Rephrase, "rephrased wording");
		add_reason(
			g.get_mut(&kid).unwrap(),
			Reason {
				id: rid.clone(),
				from: "old".into(),
				to: String::new(),
				kind: ReasonKind::Rephrase,
				text: "rephrased wording".into(),
				..Default::default()
			},
		);
		g.set_source_entry("ext1".into(), kid.clone());

		supersede(&mut g, &kid, "new", &[1.0, 0.0], "ext1", "replaced");

		// the candidate is re-pointed to `new` and queued for re-classification
		let kern = g.loaded(&kid).unwrap();
		let r = kern.reasons.get(&rid).expect("rephrase edge kept");
		assert_eq!(r.from, "new", "re-pointed to the new active entity");
		assert!(r.to.is_empty(), "still a deferred candidate");
		let queued = g.drain_pending_reclass();
		assert!(
			queued.iter().any(|(k, r)| k == &kid && r == &rid),
			"queued for re-classification: {queued:?}"
		);
	}

	#[test]
	fn supersede_drops_the_old_entity_from_the_search_index() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let old = Entity {
			id: "old".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			..Default::default()
		};
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("old".into(), old);
			k.source_index.insert("ext1".into(), "old".into());
		}
		g.index_entity("old", &kid);
		g.set_source_entry("ext1".into(), kid.clone());

		let before: Vec<String> = search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			before.contains(&"old".to_string()),
			"old is indexed before supersede"
		);

		supersede(
			&mut g,
			&kid,
			"new",
			&[1.0, 0.0],
			"ext1",
			"replaced by newer version",
		);

		let after: Vec<String> = search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			!after.contains(&"old".to_string()),
			"superseded entity removed from search index"
		);
		let kern = g.loaded(&kid).unwrap();
		let old_e = kern
			.entities
			.get("old")
			.expect("superseded entity still stored");
		assert_eq!(
			old_e.status,
			EntityStatus::Superseded,
			"kept as Superseded history"
		);
		assert_eq!(old_e.superseded_by, "new", "supersede chain preserved");
	}

	#[test]
	fn accept_never_leaves_empty_unnamed_kern() {
		let (mut g, root, _graviton) = graph_with_graviton();
		let vectors = [
			vec![1.0, 0.0, 0.0], // matches the graviton
			vec![1.0, 0.0, 0.0], // duplicate -> deduped, must NOT spawn
			vec![0.0, 1.0, 0.0], // non-match -> generic
			vec![0.0, 1.0, 0.0], // duplicate of the generic one
			vec![0.0, 0.0, 1.0], // another non-match
			vec![0.9, 0.1, 0.0], // near the graviton
		];
		for (i, v) in vectors.iter().enumerate() {
			accept(&mut g, &root, ent(&format!("e{i}"), v.clone()), "");
		}
		let empties: Vec<String> = g
			.all()
			.iter()
			.filter(|k| k.id != root && k.is_unnamed() && k.entities.is_empty())
			.map(|k| k.id.clone())
			.collect();
		assert!(
			empties.is_empty(),
			"accept left empty unnamed kern(s) behind: {empties:?}"
		);
	}

	#[test]
	fn supersede_chain_depth_counter_increments_past_threshold() {
		let _serial = SUPERSEDE_CHAIN_TEST_MUX.lock().unwrap();
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		// Seed `ext1` with e0, then supersede six times: e1←e0, e2←e1, … e6←e5.
		// The sixth hop makes superseded_ancestors(e5) = [e4,e3,e2,e1,e0] (len 5),
		// depth 6 > SUPERSEDE_CHAIN_HOP_THRESHOLD (5) → one increment.
		let old = Entity {
			id: "e0".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			..Default::default()
		};
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("e0".into(), old);
			k.source_index.insert("ext1".into(), "e0".into());
		}
		g.set_source_entry("ext1".into(), kid.clone());
		g.index_entity("e0", &kid);

		let before = supersede_chain_depth_exceeded();
		// Insert e_i then supersede(e_i): `supersede` does not insert the new
		// entity, so the next hop's `old` lookup needs it present in the kern.
		for i in 1..=6 {
			let new_id = format!("e{i}");
			if let Some(k) = g.get_mut(&kid) {
				k.entities.insert(
					new_id.clone(),
					Entity {
						id: new_id.clone(),
						external_id: "ext1".into(),
						vector: vec![1.0, 0.0].into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
			g.index_entity(&new_id, &kid);
			supersede(&mut g, &kid, &new_id, &[1.0, 0.0], "ext1", "hop");
		}
		let delta = supersede_chain_depth_exceeded() - before;
		assert_eq!(
			delta, 1,
			"a 6-deep chain on one external_id increments the counter once"
		);

		// A fresh 3-deep chain on a different external_id stays under threshold.
		let before2 = supersede_chain_depth_exceeded();
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert(
				"s0".into(),
				Entity {
					id: "s0".into(),
					external_id: "ext2".into(),
					vector: vec![0.0, 1.0].into(),
					status: EntityStatus::Active,
					..Default::default()
				},
			);
			k.source_index.insert("ext2".into(), "s0".into());
		}
		g.set_source_entry("ext2".into(), kid.clone());
		g.index_entity("s0", &kid);
		for i in 1..=3 {
			let new_id = format!("s{i}");
			if let Some(k) = g.get_mut(&kid) {
				k.entities.insert(
					new_id.clone(),
					Entity {
						id: new_id.clone(),
						external_id: "ext2".into(),
						vector: vec![0.0, 1.0].into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
			g.index_entity(&new_id, &kid);
			supersede(&mut g, &kid, &new_id, &[0.0, 1.0], "ext2", "hop");
		}
		let delta2 = supersede_chain_depth_exceeded() - before2;
		assert_eq!(delta2, 0, "a 3-deep chain does not cross the threshold");
	}

	#[test]
	fn supersede_stamps_both_temporal_clocks() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let old = Entity {
			id: "old".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			created_at: Some(std::time::SystemTime::now()),
			..Default::default()
		};
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		let new_from = std::time::SystemTime::now();
		let new = Entity {
			id: "new".into(),
			external_id: "ext1".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			valid_from: Some(new_from),
			..Default::default()
		};
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("old".into(), old);
			k.entities.insert("new".into(), new);
			k.source_index.insert("ext1".into(), "old".into());
		}
		g.index_entity("old", &kid);
		g.index_entity("new", &kid);
		g.set_source_entry("ext1".into(), kid.clone());

		supersede(&mut g, &kid, "new", &[1.0, 0.0], "ext1", "temporal test");

		let kern = g.loaded(&kid).unwrap();
		let old_e = kern.entities.get("old").unwrap();
		assert_eq!(old_e.status, EntityStatus::Superseded);
		assert!(
			old_e.invalidated_at.is_some(),
			"transaction-time stamp recorded"
		);
		assert_eq!(
			old_e.valid_to,
			Some(new_from),
			"old window closes at the successor's valid_from"
		);
		assert!(
			!old_e.is_valid_at(new_from),
			"old is no longer valid at the successor's start instant"
		);
	}

	#[test]
	fn contradiction_supersede_materializes_new_and_invalidates_old() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let old = Entity {
			id: "old".into(),
			vector: vec![1.0, 0.0].into(),
			status: EntityStatus::Active,
			created_at: Some(std::time::SystemTime::now()),
			..Default::default()
		};
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert("old".into(), old);
		}
		g.index_entity("old", &kid);

		let new = Entity {
			id: "new".into(),
			vector: vec![0.99, 0.01].into(),
			status: EntityStatus::Active,
			created_at: Some(std::time::SystemTime::now()),
			..Default::default()
		};
		let rids = supersede_by_contradiction(&mut g, &kid, "old", new, "contradicts earlier claim");
		assert_eq!(rids.len(), 1, "one Supersedes edge minted");

		let kern = g.loaded(&kid).unwrap();
		let sup_r = kern.reasons.get(&rids[0]).expect("supersede reason exists");
		assert_eq!(
			sup_r.text, "contradicts earlier claim",
			"reason text stored"
		);

		let kern = g.loaded(&kid).unwrap();
		assert!(
			kern.entities.contains_key("new"),
			"new revision materialized"
		);
		let old_e = kern.entities.get("old").unwrap();
		assert_eq!(old_e.status, EntityStatus::Superseded);
		assert_eq!(old_e.superseded_by, "new");
		assert!(old_e.invalidated_at.is_some(), "old stamped invalidated");

		let hits: Vec<String> = search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(!hits.contains(&"old".to_string()), "old evicted from ANN");
		assert!(hits.contains(&"new".to_string()), "new revision indexed");
	}

	#[test]
	fn contradiction_supersede_is_a_noop_on_missing_or_already_superseded() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let new = Entity {
			id: "new".into(),
			vector: vec![1.0, 0.0].into(),
			..Default::default()
		};
		assert!(supersede_by_contradiction(&mut g, &kid, "ghost", new, "missing old").is_empty());
	}

	#[test]
	fn parse_contradiction_fails_open_to_related() {
		assert_eq!(parse_contradiction("UPDATE"), ContradictionClass::Supersede);
		assert_eq!(
			parse_contradiction("  contradiction \n"),
			ContradictionClass::Supersede
		);
		assert_eq!(parse_contradiction("RELATED"), ContradictionClass::Related);
		assert_eq!(parse_contradiction(""), ContradictionClass::Related);
		assert_eq!(
			parse_contradiction("I'm not sure"),
			ContradictionClass::Related
		);
		assert_eq!(
			parse_contradiction("this is an update but they are RELATED"),
			ContradictionClass::Related,
			"a RELATED mention wins — conservative"
		);
	}

	#[test]
	fn resolve_valid_until_is_a_min_with_none_as_infinity() {
		use std::time::{Duration, UNIX_EPOCH};
		let early = UNIX_EPOCH + Duration::from_secs(100);
		let late = UNIX_EPOCH + Duration::from_secs(500);

		assert_eq!(resolve_valid_until(Some(late), Some(early)), Some(early));
		assert_eq!(
			resolve_valid_until(Some(early), Some(late)),
			Some(early),
			"commutative — the shorter deadline wins in either order"
		);
		assert_eq!(
			resolve_valid_until(None, Some(early)),
			Some(early),
			"min(∞, t) = t — a never-expiring entity accepts a deadline"
		);
		assert_eq!(
			resolve_valid_until(Some(early), None),
			Some(early),
			"min(t, ∞) = t — no opinion never lengthens a deadline"
		);
		assert_eq!(resolve_valid_until(None, None), None);
		assert_eq!(
			resolve_valid_until(Some(early), Some(early)),
			Some(early),
			"idempotent"
		);
	}

	// The incremental sibling of `rebuild_index_shares_the_map_s_vector_allocation`:
	// `commit_entity` indexes on insert rather than waiting for a rebuild, and it
	// has to hand the index the entity's own allocation too or a live graph pays
	// the second copy back one entity at a time.
	#[test]
	fn commit_entity_indexes_the_entity_s_own_vector_allocation() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let r = accept(&mut g, &root, ent("a", vec![1.0, 0.0, 0.0]), "");
		assert!(
			!r.deduped,
			"the fixture only holds while the entity is placed"
		);
		let kid = g.kern_of_entity(&r.entity_id).expect("indexed").to_string();
		let e = &g.loaded(&kid).expect("kern").entities[&r.entity_id];
		assert_eq!(
			std::sync::Arc::strong_count(&e.vector),
			2,
			"entity_idx must share the committed entity's vector, not copy it"
		);
	}

	#[test]
	fn duplicate_vector_is_deduped() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let r1 = accept(&mut g, &root, ent("a", vec![1.0, 0.0, 0.0]), "");
		assert!(!r1.deduped, "first entity is placed, not deduped");
		let r2 = accept(&mut g, &root, ent("b", vec![1.0, 0.0, 0.0]), "");
		assert!(r2.deduped, "identical vector must dedup");
	}

	fn ent_text(id: &str, vector: Vec<f32>, text: &str) -> Entity {
		Entity {
			id: id.into(),
			vector: vector.into(),
			statements: vec![text.into()],
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::StatementRef,
				index: 0,
				text: String::new(),
			}],
			..Default::default()
		}
	}

	fn survivor<'a>(g: &'a GraphGnn, id: &str) -> &'a Entity {
		let kid = g.kern_of_entity(id).expect("survivor is indexed");
		g.loaded(kid).unwrap().entities.get(id).unwrap()
	}

	#[test]
	fn accept_time_duplicate_merges_instead_of_dropping() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let r1 = accept(
			&mut g,
			&root,
			ent_text("a", vec![1.0, 0.0, 0.0], "the claim"),
			"",
		);
		assert!(!r1.deduped);
		let before = survivor(&g, "a").clone();

		let r2 = accept(
			&mut g,
			&root,
			ent_text("b", vec![1.0, 0.0, 0.0], "the claim reworded"),
			"",
		);
		assert!(r2.deduped, "identical vector must dedup");
		assert_eq!(
			r2.entity_id, "a",
			"result names the SURVIVOR, not the dropped incoming id"
		);

		let after = survivor(&g, "a");
		assert!(
			after.conf_alpha > before.conf_alpha,
			"duplicate corroborates the survivor instead of vanishing"
		);
		assert!(after.updated_at.is_some(), "updated_at bumped");
		assert!(
			!g.loaded(&r2.placed_in).unwrap().entities.contains_key("b"),
			"the duplicate is not stored under its own id"
		);
	}

	#[test]
	fn accept_time_duplicate_records_rephrase_edge() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		accept(
			&mut g,
			&root,
			ent_text("a", vec![1.0, 0.0, 0.0], "the claim"),
			"",
		);
		let r = accept(
			&mut g,
			&root,
			ent_text("b", vec![1.0, 0.0, 0.0], "the claim reworded"),
			"",
		);

		let kid = g.kern_of_entity("a").unwrap();
		let rephrase: Vec<_> = g
			.loaded(kid)
			.unwrap()
			.reasons
			.values()
			.filter(|x| x.kind == ReasonKind::Rephrase)
			.collect();
		assert_eq!(rephrase.len(), 1, "exactly one rephrase edge");
		assert_eq!(rephrase[0].from, "a", "annotated on the survivor");
		assert_eq!(
			rephrase[0].text, "the claim reworded",
			"alternate phrasing preserved"
		);
		assert_eq!(
			r.reason_ids,
			vec![rephrase[0].id.clone()],
			"merge reports the edge it minted"
		);
	}

	#[test]
	fn accept_time_merge_never_overwrites_survivor_text_or_vector() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		accept(
			&mut g,
			&root,
			ent_text("a", vec![1.0, 0.0, 0.0], "the claim"),
			"",
		);
		let before = survivor(&g, "a").clone();

		accept(
			&mut g,
			&root,
			ent_text("b", vec![1.0, 0.0, 0.0], "a totally different wording"),
			"",
		);

		let after = survivor(&g, "a");
		assert_eq!(after.id, "a", "content-addressed id unchanged");
		assert_eq!(after.text(), before.text(), "stored text NOT overwritten");
		assert_eq!(
			after.statements, before.statements,
			"statements NOT overwritten"
		);
		assert_eq!(after.vector, before.vector, "vector NOT overwritten");
	}

	#[test]
	fn distinct_vector_is_placed() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		accept(&mut g, &root, ent("a", vec![1.0, 0.0, 0.0]), "");
		let r = accept(&mut g, &root, ent("c", vec![0.0, 1.0, 0.0]), "");
		assert!(!r.deduped, "orthogonal vector must not dedup");
	}

	fn graph_with_graviton() -> (GraphGnn, String, String) {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();
		let graviton = Kern::new_named_child(&root, &root_net, "work", vec![1.0, 0.0, 0.0]);
		let graviton_id = graviton.id.clone();
		g.register(graviton);
		g.get_mut(&root).unwrap().children.push(graviton_id.clone());
		(g, root, graviton_id)
	}

	#[test]
	fn routes_nonmatch_to_generic() {
		let (mut g, root, graviton_id) = graph_with_graviton();
		let r = accept(&mut g, &root, ent("e", vec![0.0, 1.0, 0.0]), "");
		assert_ne!(
			r.placed_in, root,
			"must not commit onto the root dispatcher"
		);
		assert_ne!(
			r.placed_in, graviton_id,
			"non-matching entity must not enter the graviton"
		);
		let placed = g.loaded(&r.placed_in).expect("placed kern is loaded");
		assert_eq!(
			placed.graviton_text, GENERIC_GRAVITON,
			"fell through to generic"
		);
	}

	#[test]
	fn routes_match_to_graviton() {
		let (mut g, root, graviton_id) = graph_with_graviton();
		let r = accept(&mut g, &root, ent("e", vec![1.0, 0.0, 0.0]), "");
		assert_eq!(
			r.placed_in, graviton_id,
			"matching entity enters its graviton"
		);
	}

	// ponytail: the per-descent children clone is gone — routing through a root
	// with 4 named children allocates no more than through a root with 1, since
	// `&kern.children` is held alongside the `&GraphGnn` reborrow (item 31).
	#[test]
	fn route_entity_does_not_clone_children_per_descent() {
		use test_support::alloc_probe;

		let build = |n: usize| -> (GraphGnn, String) {
			let mut g = GraphGnn::new();
			let root = g.root.id.clone();
			let root_net = g.root.root_id.clone();
			for i in 0..n {
				let name = format!("work{i}");
				let mut v = vec![0.0, 0.0, 0.0];
				v[i % 3] = 1.0;
				let k = Kern::new_named_child(&root, &root_net, &name, v);
				g.get_mut(&root).unwrap().children.push(k.id.clone());
				g.register(k);
			}
			(g, root)
		};

		let thought = ent("e", vec![1.0, 0.0, 0.0]);
		let (mut g4, root4) = build(4);
		let (mut g1, root1) = build(1);

		let (_, a4) = alloc_probe::measure(|| route_entity(&mut g4, &root4, &thought, false));
		let (_, a1) = alloc_probe::measure(|| route_entity(&mut g1, &root1, &thought, false));
		// The matched-id String clone is the only alloc left and it is the same
		// length in both; the children Vec<String> clone is gone, so the two agree
		// within a tight tolerance. A re-added `.clone()` of 4 vs 1 children would
		// push a4 roughly 3 String-headers (~72 B) past a1 and red this.
		let diff = a4.total as i64 - a1.total as i64;
		assert!(
			diff.abs() <= 8,
			"children clone leaked: a4={a4:?} a1={a1:?} diff={diff}"
		);
	}

	fn graviton_names(g: &GraphGnn) -> Vec<String> {
		root_graviton_ids(g)
			.iter()
			.filter_map(|c| g.loaded(c))
			.map(|k| k.graviton_text.clone())
			.collect()
	}

	#[test]
	fn add_graviton_creates_named_root_child() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		assert!(graviton_names(&g).contains(&"work".to_string()));
		let r = accept(&mut g, &root, ent("e", vec![1.0, 0.0, 0.0]), "");
		assert!(
			g.loaded(&r.placed_in)
				.map(|k| k.graviton_text == "work")
				.unwrap_or(false),
			"matching entity enters the added graviton"
		);
	}

	#[test]
	fn remove_graviton_demotes_and_reports() {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		assert!(remove_graviton(&mut g, "work"), "existing graviton removed");
		assert!(
			!graviton_names(&g).contains(&"work".to_string()),
			"graviton no longer a named root child"
		);
		assert!(
			!remove_graviton(&mut g, "missing"),
			"missing graviton -> false"
		);
	}

	#[test]
	fn promote_skips_when_root_has_equivalent_graviton_by_name() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "sessions with no parent", vec![1.0, 0.0, 0.0], 1.0);
		let generic = get_or_spawn_generic_child(&mut g, &root);
		let root_net = g.root.root_id.clone();
		let child = Kern::new_named_child(
			&generic,
			&root_net,
			" Sessions With No Parent ",
			vec![0.0, 1.0, 0.0],
		);
		let cid = child.id.clone();
		g.register(child);
		g.get_mut(&generic).unwrap().children.push(cid.clone());

		assert!(
			!promote_to_root_if_generic(&mut g, &cid),
			"name-equivalent graviton exists -> no promotion"
		);
		assert!(
			!root_graviton_ids(&g).contains(&cid),
			"not minted as a root graviton"
		);
		assert_eq!(
			g.loaded(&cid).unwrap().parent,
			generic,
			"stays under generic"
		);
	}

	#[test]
	fn promote_skips_when_root_graviton_vec_is_near_duplicate() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "parentless sessions", vec![1.0, 0.0, 0.0], 1.0);
		let generic = get_or_spawn_generic_child(&mut g, &root);
		let root_net = g.root.root_id.clone();

		let near = Kern::new_named_child(
			&generic,
			&root_net,
			"sessions without parents",
			vec![1.0, 0.1, 0.0],
		);
		let near_id = near.id.clone();
		g.register(near);
		g.get_mut(&generic).unwrap().children.push(near_id.clone());
		assert!(
			!promote_to_root_if_generic(&mut g, &near_id),
			"vector-equivalent graviton exists -> no promotion"
		);

		let fresh = Kern::new_named_child(&generic, &root_net, "shader pipelines", vec![0.0, 0.0, 1.0]);
		let fresh_id = fresh.id.clone();
		g.register(fresh);
		g.get_mut(&generic).unwrap().children.push(fresh_id.clone());
		assert!(
			promote_to_root_if_generic(&mut g, &fresh_id),
			"orthogonal concept still promotes"
		);
	}

	#[test]
	fn heavier_graviton_wins_at_equal_distance() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "light", vec![1.0, 0.0, 0.0], 1.0);
		add_graviton_with_mass(&mut g, "heavy", vec![0.0, 1.0, 0.0], 2.0);

		let r = accept(&mut g, &root, ent("e", vec![1.0, 1.0, 0.0]), "");
		assert_eq!(
			g.loaded(&r.placed_in).unwrap().graviton_text,
			"heavy",
			"equal cosine distance, larger mass -> smaller effective distance -> wins"
		);
	}

	#[test]
	fn default_mass_preserves_nearest_graviton_routing() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		add_graviton_with_mass(&mut g, "near", vec![1.0, 0.0, 0.0], 1.0);
		add_graviton_with_mass(&mut g, "far", vec![0.0, 1.0, 0.0], 1.0);
		for id in root_graviton_ids(&g) {
			assert_eq!(g.loaded(&id).unwrap().mass, 1.0, "default mass is 1.0");
		}

		let r = accept(&mut g, &root, ent("e", vec![0.95, 0.05, 0.0]), "");
		assert_eq!(
			g.loaded(&r.placed_in).unwrap().graviton_text,
			"near",
			"mass 1.0 everywhere reproduces plain nearest-distance routing"
		);
	}

	#[test]
	fn seed_examples_splits_lines_and_keeps_single_text_whole() {
		assert_eq!(
			seed_examples("one example.\n  two example.  \n\nthree."),
			vec!["one example.", "two example.", "three."]
		);
		assert_eq!(
			seed_examples("a single description with no newlines"),
			vec!["a single description with no newlines"]
		);
		assert_eq!(
			seed_examples("  padded single line  \n"),
			vec!["padded single line"],
			"one non-empty line embeds whole, not as a one-element pool"
		);
	}

	#[test]
	fn seed_examples_char_chunks_a_long_single_paragraph() {
		let chunk = base::base_constants::GRAVITON_SEED_CHAR_CHUNK;
		let body = "x".repeat(chunk + 5);
		let out = seed_examples(&body);
		assert_eq!(out.len(), 2, "ceil((chunk+5)/chunk) -> 2 chunks");
		assert!(out.iter().all(|c| c.chars().count() <= chunk));
		assert_eq!(
			out.concat(),
			body,
			"chunks reassemble to the trimmed original"
		);
		// exactly-on-boundary: chunk chars -> one chunk (not two)
		assert_eq!(seed_examples(&"x".repeat(chunk)).len(), 1);
	}

	#[test]
	fn seed_examples_char_chunks_split_on_a_code_point_boundary() {
		// a multibyte char straddling the boundary must not be split mid-char
		let chunk = base::base_constants::GRAVITON_SEED_CHAR_CHUNK;
		let mut body = "a".repeat(chunk - 1);
		body.push('ß');
		body.push('z');
		let out = seed_examples(&body);
		// ß is one char, so chunk-1 'a' + 'ß' fills chunk 1; 'z' is chunk 2
		assert_eq!(out.len(), 2);
		assert_eq!(out.concat(), body);
		assert!(out.iter().all(|c| c.chars().count() <= chunk));
	}

	#[test]
	fn mean_pool_normalizes_and_rejects_mismatched_dims() {
		let v = mean_pool(&[vec![1.0, 0.0], vec![0.0, 1.0]]).unwrap();
		let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
		assert!((norm - 1.0).abs() < 1e-6, "pooled vector is unit-norm");
		assert!((v[0] - v[1]).abs() < 1e-6, "equal contribution");
		assert!(mean_pool(&[]).is_none());
		assert!(mean_pool(&[vec![1.0, 0.0], vec![1.0]]).is_none());
		assert!(
			mean_pool(&[vec![1.0, 0.0], vec![-1.0, 0.0]]).is_none(),
			"opposite examples cancel to zero — refuse rather than emit garbage"
		);
	}

	// ROADMAP item 84: `promote_unnamed` gives an existing unnamed kern a
	// graviton in place — no move, no id change, no re-register — so it becomes
	// `is_named` and gc keeps it.
	#[test]
	fn promote_unnamed_adds_a_graviton_in_place() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let root_net = g.root.root_id.clone();
		let child = Kern::new_unnamed(&root, &root_net);
		let cid = child.id.clone();
		g.register(child);
		assert!(
			g.loaded(&cid).unwrap().is_unnamed(),
			"precondition: unnamed"
		);

		promote_unnamed(&mut g, &cid, "pinned", vec![1.0, 0.0], 2.0).unwrap();

		let k = g.loaded(&cid).unwrap();
		assert!(k.is_named(), "now named (has a graviton)");
		assert!(k.has_graviton(), "text + vec set");
		assert_eq!(k.graviton_text, "pinned");
		assert_eq!(k.mass, 2.0);
		assert_eq!(k.id, cid, "no id change");
		assert_eq!(k.parent, root, "no move");
	}

	#[test]
	fn promote_unnamed_rejects_a_named_or_missing_kern() {
		let mut g = GraphGnn::new();
		// missing
		assert!(promote_unnamed(&mut g, "ghost", "x", vec![1.0, 0.0], 1.0).is_err());
		// already named
		add_graviton_with_mass(&mut g, "docs", vec![1.0, 0.0, 0.0], 1.0);
		let id = find_graviton_by_name(&g, "docs").unwrap();
		assert!(
			promote_unnamed(&mut g, &id, "dup", vec![1.0, 0.0], 1.0).is_err(),
			"a named kern is not a promote target"
		);
	}

	#[test]
	fn add_graviton_with_mass_round_trips_and_updates_in_place() {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "docs", vec![1.0, 0.0, 0.0], 3.0);
		let id = find_graviton_by_name(&g, "docs").unwrap();
		assert_eq!(g.loaded(&id).unwrap().mass, 3.0, "mass stored on add");

		add_graviton_with_mass(&mut g, "docs", vec![0.0, 1.0, 0.0], 0.5);
		assert_eq!(
			g.loaded(&id).unwrap().mass,
			0.5,
			"same-name add updates mass in place"
		);
	}

	#[test]
	fn add_graviton_updates_existing_same_name_instead_of_minting_duplicate() {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		add_graviton_with_mass(&mut g, "work", vec![0.0, 1.0, 0.0], 1.0);

		let ids: Vec<String> = root_graviton_ids(&g)
			.into_iter()
			.filter(|cid| {
				g.loaded(cid)
					.map(|c| c.graviton_text == "work")
					.unwrap_or(false)
			})
			.collect();
		assert_eq!(ids.len(), 1, "one graviton per name, not one per call");
		let vec = g.loaded(&ids[0]).unwrap().graviton_vec.clone();
		assert_eq!(
			vec,
			vec![0.0, 1.0, 0.0],
			"second call updates the routing vector in place"
		);
	}

	#[test]
	fn promotes_generic_child_to_root() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let generic = get_or_spawn_generic_child(&mut g, &root);
		let root_net = g.root.root_id.clone();
		let child = Kern::new_named_child(&generic, &root_net, "shaders", vec![1.0, 0.0, 0.0]);
		let cid = child.id.clone();
		g.register(child);
		g.get_mut(&generic).unwrap().children.push(cid.clone());

		assert!(
			promote_to_root_if_generic(&mut g, &cid),
			"promoted out of generic"
		);
		assert!(
			root_graviton_ids(&g).contains(&cid),
			"now a root-level graviton"
		);
		assert_eq!(
			g.loaded(&cid).unwrap().parent,
			root,
			"parent rewired to root"
		);
		assert!(
			!g.loaded(&generic).unwrap().children.contains(&cid),
			"detached from generic"
		);
		assert!(
			!promote_to_root_if_generic(&mut g, &cid),
			"idempotent once at root level"
		);
	}
}
