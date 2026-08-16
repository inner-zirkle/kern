//! Tests extracted from graph.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{Entity, Reason};

	fn empty_unnamed(id: &str, parent: &str, children: &[&str]) -> Kern {
		let mut k = Kern::new(id, parent);
		k.children = children.iter().map(|s| s.to_string()).collect();
		k
	}

	#[test]
	fn query_dim_guard_follows_the_dominant_indexed_dimension() {
		let vecs = |g: &mut GraphGnn, dims: &[(&str, usize)]| {
			let root = g.root.id.clone();
			let mut k = Kern::new("k1", &root);
			for (id, dim) in dims {
				k.entities.insert(
					(*id).into(),
					Entity {
						id: (*id).into(),
						vector: vec![0.5; *dim].into(),
						..Default::default()
					},
				);
			}
			g.kerns.insert("k1".into(), k);
			g.rebuild_index();
		};

		let mut g = GraphGnn::new();
		assert_eq!(g.entity_vector_dim(), None, "nothing indexed yet");
		assert!(
			g.query_dim_ok(&[0.1, 0.2]),
			"an unknown dimension never blocks a query"
		);

		vecs(&mut g, &[("a", 4), ("b", 4), ("c", 3)]);
		assert_eq!(g.entity_vector_dim(), Some(4), "the majority length wins");
		assert!(g.query_dim_ok(&[0.0; 4]));
		assert!(
			!g.query_dim_ok(&[0.0; 3]),
			"a query from another embedding model scores as noise — flag it"
		);
	}

	#[test]
	fn superseded_vectors_never_decide_the_indexed_dimension() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut k = Kern::new("k1", &root);
		// The index skips Superseded, so a supersede-heavy store must not report the
		// dimension of vectors the index does not hold — every query would be rejected.
		for i in 0..5 {
			k.entities.insert(
				format!("old{i}"),
				Entity {
					id: format!("old{i}"),
					status: EntityStatus::Superseded,
					vector: vec![0.5; 3].into(),
					..Default::default()
				},
			);
		}
		k.entities.insert(
			"live".into(),
			Entity {
				id: "live".into(),
				vector: vec![0.5; 4].into(),
				..Default::default()
			},
		);
		g.kerns.insert("k1".into(), k);
		g.rebuild_index();

		assert_eq!(
			g.entity_vector_dim(),
			Some(4),
			"only searchable entities define the dimension"
		);
		assert!(
			g.query_dim_ok(&[0.0; 4]),
			"a legitimate query is not rejected"
		);
	}

	#[test]
	fn unload_without_a_store_keeps_the_kern_resident() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		g.kerns.insert("k1".into(), Kern::new("k1", &root));

		g.unload("k1").expect("no store is not an error");

		assert!(
			g.kerns.contains_key("k1"),
			"without a store there is nothing to reload from, so unloading would lose the kern"
		);
		assert!(!g.unloaded.contains("k1"), "not marked unloaded either");
	}

	#[test]
	fn rebuild_index_is_deterministic_across_instances() {
		use base::base_types::Reason;
		let vec_of = |i: usize, off: f64| -> Vec<f32> {
			(0..8)
				.map(|j| ((i as f64) * (0.11 + 0.05 * j as f64) + off).sin() as f32)
				.collect()
		};
		let make_kern = |k: usize| -> Kern {
			let mut kern = Kern::new(format!("k{k}"), "root");
			for e in 0..40 {
				let id = format!("k{k}e{e}");
				kern.entities.insert(
					id.clone(),
					Entity {
						id,
						vector: vec_of(k * 100 + e, 0.0).into(),
						gnn_vector: vec_of(k * 100 + e, 0.5).into(),
						..Default::default()
					},
				);
			}
			for r in 0..10 {
				let id = format!("k{k}r{r}");
				kern.reasons.insert(
					id.clone(),
					Reason {
						id,
						vector: vec_of(k * 100 + r, 1.0).into(),
						..Default::default()
					},
				);
			}
			kern
		};
		let digest = |be: &VectorBackend| match be {
			VectorBackend::Resident(h) => h.structure_digest(),
			VectorBackend::Disk { .. } => unreachable!("test graphs never spill"),
		};
		let mut a = GraphGnn::new();
		for k in 0..5 {
			let kern = make_kern(k);
			a.kerns.insert(kern.id.clone(), kern);
		}
		let mut b = GraphGnn::new();
		for k in (0..5).rev() {
			let kern = make_kern(k);
			b.kerns.insert(kern.id.clone(), kern);
		}
		a.rebuild_index();
		b.rebuild_index();
		assert_eq!(
			digest(&a.entity_idx),
			digest(&b.entity_idx),
			"entity index structure differs across instances"
		);
		assert_eq!(
			digest(&a.gnn_entity_idx),
			digest(&b.gnn_entity_idx),
			"gnn index structure differs across instances"
		);
		assert_eq!(
			digest(&a.reason_idx),
			digest(&b.reason_idx),
			"reason index structure differs across instances"
		);
	}

	#[test]
	fn rebuild_index_excludes_superseded_entities() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert(
				"active".into(),
				Entity {
					id: "active".into(),
					vector: vec![1.0, 0.0].into(),
					status: EntityStatus::Active,
					..Default::default()
				},
			);
			k.entities.insert(
				"dead".into(),
				Entity {
					id: "dead".into(),
					vector: vec![1.0, 0.0].into(),
					status: EntityStatus::Superseded,
					..Default::default()
				},
			);
		}
		g.rebuild_index();
		let hits: Vec<String> = crate::search::search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			hits.contains(&"active".to_string()),
			"active entity is indexed"
		);
		assert!(
			!hits.contains(&"dead".to_string()),
			"superseded entity excluded from rebuilt index"
		);
	}

	#[test]
	fn disk_index_snapshot_mirrors_in_ram_membership_and_ranking() {
		// Vectors use distinct per-dim frequencies so the nearest-neighbour structure
		// is unambiguous despite in-RAM int8 quant noise vs raw f32 on disk.
		use crate::diskann::DiskIndex;
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let vec_of = |i: usize| -> Vec<f32> {
			(0..8)
				.map(|j| ((i as f64) * (0.13 + 0.07 * j as f64)).sin() as f32)
				.collect()
		};
		if let Some(k) = g.get_mut(&kid) {
			for i in 0..80 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						vector: vec_of(i).into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
			k.entities.insert(
				"dead".into(),
				Entity {
					id: "dead".into(),
					vector: vec_of(3).into(),
					status: EntityStatus::Superseded,
					..Default::default()
				},
			);
		}
		g.rebuild_index();

		let dir = tempfile::tempdir().unwrap();
		let written = g.build_entity_disk_index(dir.path()).unwrap();
		assert_eq!(
			written, 80,
			"snapshot holds all 80 active entities; superseded excluded"
		);

		let disk = DiskIndex::open(dir.path()).unwrap();
		let q32 = vec_of(40);

		let ram: Vec<String> = crate::search::search_all_unlocked(&g, &q32, 10)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		let disk_hits: Vec<String> = disk
			.search_hits_filtered(&q32, 10, 96, &|_| true)
			.into_iter()
			.map(|h| h.id)
			.collect();

		assert_eq!(
			disk_hits.first().map(String::as_str),
			Some("e40"),
			"indexed query point ranks first on disk"
		);
		assert_eq!(
			ram.first().map(String::as_str),
			Some("e40"),
			"indexed query point ranks first in RAM"
		);
		assert!(
			!disk_hits.contains(&"dead".to_string()),
			"superseded entity absent from disk snapshot"
		);

		let ram_set: std::collections::HashSet<&String> = ram.iter().collect();
		let overlap = disk_hits.iter().filter(|id| ram_set.contains(id)).count();
		assert!(
			overlap >= 6,
			"disk vs in-RAM top-10 overlap too low: {overlap}/10 (ram={ram:?} disk={disk_hits:?})"
		);
	}

	fn vec8(i: usize) -> Vec<f32> {
		(0..8)
			.map(|j| ((i as f64) * (0.13 + 0.07 * j as f64)).sin() as f32)
			.collect()
	}

	#[test]
	fn rebuild_index_spills_entity_index_to_disk_above_threshold() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		let kid = g.root.id.clone();
		if let Some(k) = g.get_mut(&kid) {
			for i in 0..40 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						vector: vec8(i).into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
		}

		g.rebuild_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Resident(_)),
			"default threshold keeps the in-RAM index"
		);

		g.set_disk_threshold(10);
		g.rebuild_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Disk { .. }),
			"entity index spilled to disk above threshold"
		);
		assert!(
			dir
				.path()
				.join("diskann")
				.join("entity")
				.join("meta.bin")
				.exists(),
			"on-disk snapshot written"
		);
		// RECALL_PLAN F4: the gnn and reason indexes spill alongside entity now,
		// so a store load rebuilds none of the three HNSW indexes.
		assert!(matches!(g.gnn_entity_idx, VectorBackend::Disk { .. }));
		assert!(matches!(g.reason_idx, VectorBackend::Disk { .. }));

		let hits = crate::search::search_all_unlocked(&g, &vec8(7), 5);
		assert_eq!(
			hits.first().map(|h| h.entity_id.clone()),
			Some("e7".into()),
			"disk-backed search returns the query point first"
		);
		assert!(
			g.kern_of_entity("e7").is_some(),
			"reverse map populated despite skipped entity insert"
		);
	}

	#[test]
	fn rebuild_index_never_spills_without_a_data_dir() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		if let Some(k) = g.get_mut(&kid) {
			for i in 0..20 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						vector: vec8(i).into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
		}
		g.set_disk_threshold(1);
		g.rebuild_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Resident(_)),
			"no data_dir -> never spill (nowhere to write)"
		);
	}

	#[test]
	fn consolidate_folds_delta_into_snapshot_and_resets_it() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		let kid = g.root.id.clone();
		if let Some(k) = g.get_mut(&kid) {
			for i in 0..30 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						vector: vec8(i).into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
		}
		g.set_disk_threshold(10);
		g.rebuild_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Disk { .. }),
			"spilled to disk"
		);
		assert_eq!(
			g.pending_disk_delta_len(),
			0,
			"fresh snapshot has an empty delta"
		);

		// Mirror the live path: source of truth AND the index/delta both get the write.
		if let Some(k) = g.get_mut(&kid) {
			for i in 100..115 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						vector: vec8(i).into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
		}
		for i in 100..115 {
			g.entity_idx.insert(format!("e{i}"), vec8(i).into());
		}
		assert_eq!(
			g.pending_disk_delta_len(),
			15,
			"post-snapshot inserts buffered in the delta"
		);

		g.consolidate_disk_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Disk { .. }),
			"still disk-backed after consolidate"
		);
		assert_eq!(
			g.pending_disk_delta_len(),
			0,
			"delta folded into the rebuilt snapshot"
		);

		let new_hit = crate::search::search_all_unlocked(&g, &vec8(108), 5);
		assert_eq!(
			new_hit.first().map(|h| h.entity_id.clone()),
			Some("e108".into()),
			"folded-in entity searchable"
		);
		let old_hit = crate::search::search_all_unlocked(&g, &vec8(5), 5);
		assert_eq!(
			old_hit.first().map(|h| h.entity_id.clone()),
			Some("e5".into()),
			"original entity still searchable"
		);
	}

	#[test]
	fn consolidate_is_a_noop_for_a_resident_index() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		if let Some(k) = g.get_mut(&kid) {
			k.entities.insert(
				"a".into(),
				Entity {
					id: "a".into(),
					vector: vec8(1).into(),
					status: EntityStatus::Active,
					..Default::default()
				},
			);
		}
		g.rebuild_index();
		g.consolidate_disk_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Resident(_)),
			"resident index untouched"
		);
		assert_eq!(g.pending_disk_delta_len(), 0);
	}

	#[test]
	fn stale_snapshot_reconciles_diff_into_delta_on_reload() {
		// RECALL_PLAN F4: a changed store loads the OLD snapshot and folds the
		// diff into the delta overlay instead of rebuilding the whole Vamana
		// graph — new entities searchable, changed vectors win, removed ids gone.
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		let kid = g.root.id.clone();
		if let Some(k) = g.get_mut(&kid) {
			for i in 0..30 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						vector: vec8(i).into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
		}
		g.set_disk_threshold(10);
		g.rebuild_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Disk { .. }),
			"spilled"
		);

		// Store changed: five new entities, one re-embedded, one removed.
		if let Some(k) = g.get_mut(&kid) {
			for i in 100..105 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						vector: vec8(i).into(),
						status: EntityStatus::Active,
						..Default::default()
					},
				);
			}
			k.entities.insert(
				"e0".into(),
				Entity {
					id: "e0".into(),
					vector: vec8(500).into(),
					status: EntityStatus::Active,
					..Default::default()
				},
			);
			k.entities.remove("e1");
		}
		g.rebuild_index();

		let new_hit = crate::search::search_all_unlocked(&g, &vec8(103), 5);
		assert_eq!(
			new_hit.first().map(|h| h.entity_id.clone()),
			Some("e103".into()),
			"new entity searchable via the delta"
		);
		let changed = crate::search::search_all_unlocked(&g, &vec8(500), 5);
		assert_eq!(
			changed.first().map(|h| h.entity_id.clone()),
			Some("e0".into()),
			"re-embedded vector wins over the snapshot copy"
		);
		let removed = crate::search::search_all_unlocked(&g, &vec8(1), 5);
		assert!(
			!removed.iter().any(|h| h.entity_id == "e1"),
			"removed entity tombstoned out of the snapshot"
		);
	}

	#[test]
	fn gc_reaps_cyclic_empty_kerns_with_children() {
		// The spawn-runaway shape: a cycle of empty kerns with NO childless leaf —
		// do NOT simplify to a leaf-first reap, which can never start here.
		let mut g = GraphGnn::default();
		let root_id = g.root.id.clone();

		g.register(empty_unnamed("A", &root_id, &["B"]));
		g.register(empty_unnamed("B", "A", &["A"]));

		let mut named = Kern::new("N", &root_id);
		named.graviton_text = "durable facts".into();
		g.register(named);

		let mut withent = Kern::new("E", &root_id);
		withent.entities.insert(
			"e1".into(),
			Entity {
				id: "e1".into(),
				..Default::default()
			},
		);
		g.register(withent);

		if let Some(r) = g.kerns.get_mut(&root_id) {
			r.children = vec!["A".into(), "B".into(), "N".into(), "E".into()];
		}

		let before = g.kerns.len();
		let reaped = g.gc_empty_kerns();

		assert_eq!(
			reaped, 2,
			"both cyclic empty kerns reaped despite having children"
		);
		assert!(g.loaded("A").is_none(), "A reaped");
		assert!(g.loaded("B").is_none(), "B reaped");
		assert!(g.loaded("N").is_some(), "named graviton kept");
		assert!(g.loaded("E").is_some(), "entity-bearing kern kept");
		assert!(g.loaded(&root_id).is_some(), "root kept");
		assert_eq!(g.kerns.len(), before - 2);

		let root_children = &g.kerns.get(&root_id).unwrap().children;
		assert!(
			!root_children.contains(&"A".to_string()),
			"dead ref A scrubbed"
		);
		assert!(
			!root_children.contains(&"B".to_string()),
			"dead ref B scrubbed"
		);
		assert!(root_children.contains(&"N".to_string()) && root_children.contains(&"E".to_string()));
	}

	#[test]
	fn gc_keeps_empty_ancestor_on_path_to_data() {
		let mut g = GraphGnn::default();
		let root_id = g.root.id.clone();

		g.register(empty_unnamed("mid", &root_id, &["leaf"]));
		let mut leaf = Kern::new("leaf", "mid");
		leaf.entities.insert(
			"e1".into(),
			Entity {
				id: "e1".into(),
				..Default::default()
			},
		);
		g.register(leaf);
		if let Some(r) = g.kerns.get_mut(&root_id) {
			r.children = vec!["mid".into()];
		}

		let reaped = g.gc_empty_kerns();
		assert_eq!(reaped, 0, "empty ancestor of data is not reaped");
		assert!(g.loaded("mid").is_some(), "ancestor on path to data kept");
		assert!(g.loaded("leaf").is_some(), "data kern kept");
	}

	fn one_entity_one_reason() -> GraphGnn {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut k = Kern::new("k1", &root);
		k.entities.insert(
			"e1".into(),
			Entity {
				id: "e1".into(),
				vector: vec![1.0, 0.0].into(),
				gnn_vector: vec![0.0, 1.0].into(),
				..Default::default()
			},
		);
		k.reasons.insert(
			"r1".into(),
			Reason {
				id: "r1".into(),
				from: "e1".into(),
				to: "e1".into(),
				vector: vec![0.6, 0.8].into(),
				..Default::default()
			},
		);
		g.kerns.insert("k1".into(), k);
		g.rebuild_index();
		g
	}

	// ROADMAP item 83. `strong_count` is the only witness that can tell sharing
	// from copying: every assertion on length, contents or search results passes
	// just as well against a duplicate allocation, which is the thing being
	// removed. 2 = the map's handle plus the index's.
	#[test]
	fn rebuild_index_shares_the_map_s_vector_allocation_with_every_index() {
		let g = one_entity_one_reason();
		let k = g.loaded("k1").expect("k1");
		let e = &k.entities["e1"];
		let r = &k.reasons["r1"];
		assert_eq!(
			std::sync::Arc::strong_count(&e.vector),
			2,
			"entity_idx must hold the entity's own vector, not a second copy"
		);
		assert_eq!(
			std::sync::Arc::strong_count(&e.gnn_vector),
			2,
			"gnn_entity_idx must hold the entity's own gnn_vector, not a second copy"
		);
		assert_eq!(
			std::sync::Arc::strong_count(&r.vector),
			2,
			"reason_idx must hold the reason's own vector, not a second copy"
		);
	}

	// The risk sharing introduces: a write through one holder reaching the other.
	// It cannot happen — `Arc<[f32]>` has no `DerefMut`, so every write site
	// replaces its whole handle — and this pins that the index keeps answering
	// from the vector it was built with until something re-inserts it. That is
	// the same staleness window copying had, not a new one.
	#[test]
	fn replacing_an_entity_vector_does_not_reach_the_index_copy() {
		let mut g = one_entity_one_reason();
		assert_eq!(
			std::sync::Arc::strong_count(&g.loaded("k1").expect("k1").entities["e1"].vector),
			2,
			"the fixture only tests aliasing while the two holders actually share"
		);
		g.kerns
			.get_mut("k1")
			.expect("k1")
			.entities
			.get_mut("e1")
			.expect("e1")
			.vector = vec![0.0, 1.0].into();

		let hit = g.entity_idx.search(&[1.0, 0.0], 1, 10);
		assert_eq!(hit.len(), 1, "the entity is still indexed");
		assert!(
			(hit[0].score - 1.0).abs() < 1e-6,
			"the index still answers from the vector it was built with, score {}",
			hit[0].score
		);
		let e = &g.loaded("k1").expect("k1").entities["e1"];
		assert_eq!(&e.vector[..], &[0.0, 1.0], "the map holds the new vector");
		assert_eq!(
			std::sync::Arc::strong_count(&e.vector),
			1,
			"the replacement is the map's alone until a rebuild shares it"
		);
	}
}
