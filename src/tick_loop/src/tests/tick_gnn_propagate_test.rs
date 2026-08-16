//! Tests extracted from tick_gnn_propagate.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{mk_entity, EntityKind, Reason};
	use graph::reason::add_reason;

	fn kern_with_n(n: usize) -> Kern {
		let mut k = Kern::new("k", "");
		for i in 0..n {
			let id = format!("e{i}");
			k.entities
				.insert(id.clone(), mk_entity(&id, &id, 0.0, EntityKind::Claim));
		}
		for i in 0..n.saturating_sub(1) {
			let from = format!("e{i}");
			let to = format!("e{}", i + 1);
			add_reason(
				&mut k,
				Reason {
					from: from.clone(),
					to: to.clone(),
					id: format!("{from}->{to}"),
					..Default::default()
				},
			);
		}
		k
	}

	#[test]
	fn gnn_skipped_below_min_thoughts_default() {
		let k = kern_with_n(3);
		let cfg = GnnConfig::defaults();
		assert!(
			build_gnn_snapshot(&k, &cfg).is_none(),
			"3-node graph skips GNN under the default min_thoughts floor"
		);
	}

	#[test]
	fn gnn_runs_when_floor_lowered() {
		let k = kern_with_n(3);
		let mut cfg = GnnConfig::defaults();
		cfg.min_thoughts = 2;
		assert!(
			build_gnn_snapshot(&k, &cfg).is_some(),
			"with a low floor and local edges, a snapshot builds"
		);
	}

	// The negative control for sources 3 and 4 of ROADMAP item 102 — the half no
	// seed can fix. Two identically built kerns hash their keys in different
	// orders inside one process, so with the sorts reverted `ids` and
	// `pos_edges` disagree here; a seeded rng over a shuffled snapshot is still
	// a different training run.
	#[test]
	fn two_identical_kerns_snapshot_in_the_same_order() {
		let cfg = GnnConfig {
			min_thoughts: 2,
			..GnnConfig::defaults()
		};
		let a = build_gnn_snapshot(&kern_with_n(24), &cfg).expect("snapshot builds");
		let b = build_gnn_snapshot(&kern_with_n(24), &cfg).expect("snapshot builds");
		assert_eq!(a.ids, b.ids, "node order must not be hash order");
		assert_eq!(
			a.pos_edges, b.pos_edges,
			"edge order must not be hash order"
		);
		assert_eq!(a.seed, b.seed, "one corpus seeds one training run");
		let other = build_gnn_snapshot(&kern_with_n(23), &cfg).expect("snapshot builds");
		assert_ne!(
			a.seed, other.seed,
			"a different corpus gets its own seed — kerns are not initialised alike"
		);
	}

	#[test]
	fn superseded_entities_excluded_from_gnn_snapshot() {
		let mut k = kern_with_n(4);
		k.entities.get_mut("e3").unwrap().status = EntityStatus::Superseded;
		let mut cfg = GnnConfig::defaults();
		cfg.min_thoughts = 2;
		let snap = build_gnn_snapshot(&k, &cfg).expect("active e0..e2 still build a snapshot");
		assert!(
			!snap.ids.contains(&"e3".to_string()),
			"superseded leaf excluded from GNN membership"
		);
		for id in ["e0", "e1", "e2"] {
			assert!(snap.ids.contains(&id.to_string()), "active {id} included");
		}
	}

	#[test]
	fn cosine_align_maps_similarity_into_zero_one() {
		assert_eq!(
			cosine_align(&[1.0, 0.0], &[1.0, 0.0]),
			1.0,
			"identical -> 1.0"
		);
		assert_eq!(
			cosine_align(&[1.0, 0.0], &[-1.0, 0.0]),
			0.0,
			"opposite -> 0.0"
		);
		assert!(
			(cosine_align(&[1.0, 0.0], &[0.0, 1.0]) - 0.5).abs() < 1e-6,
			"orthogonal -> 0.5"
		);
		assert_eq!(cosine_align(&[], &[]), 0.5, "empty -> 0.5");
		assert_eq!(
			cosine_align(&[1.0, 2.0], &[1.0]),
			0.5,
			"length mismatch -> 0.5"
		);
		assert_eq!(
			cosine_align(&[0.0, 0.0], &[1.0, 1.0]),
			0.5,
			"zero-norm -> 0.5"
		);
	}

	#[test]
	fn a_failed_propagation_writes_nothing_and_is_recorded_as_a_degradation() {
		let mut k = kern_with_n(3);
		// Every pair is a positive edge, so no negative edge can be sampled.
		add_reason(
			&mut k,
			Reason {
				from: "e0".into(),
				to: "e2".into(),
				id: "e0->e2".into(),
				..Default::default()
			},
		);
		k.gnn_weights = vec![7, 7, 7];
		let mut g = GraphGnn::new();
		g.kerns.insert("k".into(), k);
		let g = Arc::new(RwLock::new(g));
		let q = Queue::new(16);
		let cfg = GnnConfig {
			min_thoughts: 2,
			..GnnConfig::defaults()
		};

		do_gnn_propagate(&q, &g, "k", &cfg);

		{
			let gg = g.read();
			let kern = &gg.kerns["k"];
			assert_eq!(
				kern.gnn_weights,
				vec![7, 7, 7],
				"a failed run must not persist weights over the good ones"
			);
			assert!(
				kern.entities.values().all(|e| e.gnn_vector.is_empty()),
				"no embedding is degraded by a run that never finished"
			);
		}

		let (failed, last) = q.failures();
		assert_eq!(failed, 1, "the failure is counted, not just logged");
		assert!(
			last.expect("retained").message.contains("negative edges"),
			"the last error is kept for health reporting"
		);
		let mut rx = q.take_receiver().unwrap();
		assert!(
			rx.try_recv().is_err(),
			"no Persist is enqueued when nothing changed"
		);
	}

	#[test]
	fn apply_gnn_updates_writes_gnn_vector_weights_and_enqueues_persist() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities
			.insert("e0".into(), mk_entity("e0", "e0", 0.0, EntityKind::Claim));
		g.kerns.insert("k".into(), k);
		let g = Arc::new(RwLock::new(g));

		let new_vec = vec![0.25f64, 0.5, 0.75];
		let mut updates = HashMap::new();
		updates.insert("e0".to_string(), new_vec.clone());
		let q = Queue::new(16);

		apply_gnn_updates(&q, &g, "k", updates, vec![9, 9]);

		{
			let gg = g.read();
			let kern = gg.kerns.get("k").unwrap();
			assert_eq!(
				kern.entities["e0"].gnn_vector,
				vec![0.25f32, 0.5, 0.75].into(),
				"gnn_vector overwritten (narrowed at the boundary)"
			);
			assert_eq!(kern.gnn_weights, vec![9, 9], "kern gnn_weights stored");
		}

		let mut rx = q.take_receiver().unwrap();
		let mut persisted = false;
		while let Ok(t) = rx.try_recv() {
			if matches!(t.kind, TaskKind::Persist) {
				persisted = true;
			}
		}
		assert!(persisted, "a Persist task is enqueued after updates land");
	}

	#[test]
	fn apply_gnn_updates_skips_empty_update_vectors() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities
			.insert("e0".into(), mk_entity("e0", "e0", 0.0, EntityKind::Claim));
		g.kerns.insert("k".into(), k);
		let g = Arc::new(RwLock::new(g));

		let mut updates = HashMap::new();
		updates.insert("e0".to_string(), Vec::new());
		let q = Queue::new(16);
		apply_gnn_updates(&q, &g, "k", updates, Vec::new());

		let gg = g.read();
		assert!(
			gg.kerns["k"].entities["e0"].gnn_vector.is_empty(),
			"empty update doesn't write"
		);
	}
}
