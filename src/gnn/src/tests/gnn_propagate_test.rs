//! Tests extracted from gnn_propagate.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	fn tiny_snapshot(n: usize, dim: usize) -> GnnSnapshot {
		let mut graph = Graph::new();
		for i in 0..n {
			let feats: Vec<f64> = (0..dim).map(|d| ((i + d) as f64).sin()).collect();
			graph.add_node(&format!("n{i}"), feats).unwrap();
		}
		let mut pos_edges = Vec::new();
		for i in 0..n - 1 {
			graph
				.add_edge(&format!("n{i}"), &format!("n{}", i + 1))
				.unwrap();
			pos_edges.push([i, i + 1]);
		}
		let data: Vec<f64> = (0..n * dim).map(|k| ((k as f64) * 0.1).cos()).collect();
		GnnSnapshot {
			ids: (0..n).map(|i| format!("n{i}")).collect(),
			features: Tensor::new(n, dim, data).unwrap(),
			graph,
			pos_edges,
			weights: Vec::new(),
			seed: 0xC0FFEE,
		}
	}

	#[test]
	fn empty_snapshot_is_an_error() {
		let snap = GnnSnapshot {
			ids: Vec::new(),
			features: Tensor::zeros(0, 0),
			graph: Graph::new(),
			pos_edges: Vec::new(),
			weights: Vec::new(),
			seed: 1,
		};
		let err = match run_learned_propagation(&snap, &GnnConfig::defaults()) {
			Err(e) => e,
			Ok(_) => panic!("expected error for empty snapshot"),
		};
		assert_eq!(err, "empty snapshot");
	}

	#[test]
	fn happy_path_returns_finite_updates_and_weights() {
		let dim = 8;
		let snap = tiny_snapshot(6, dim);
		let cfg = GnnConfig {
			train_epochs: 3,
			..GnnConfig::defaults()
		};
		let result = run_learned_propagation(&snap, &cfg).unwrap();

		assert_eq!(result.updates.len(), snap.ids.len());
		assert!(!result.weights.is_empty(), "weights should be marshalled");
		for id in &snap.ids {
			let v = result.updates.get(id).expect("every id has an update");
			assert_eq!(v.len(), dim);
			assert!(v.iter().all(|x| x.is_finite()), "updates must be finite");
		}
	}

	// The production path is `Model::forward`/`Model::backward`, not the `try_*`
	// layer methods: a mismatch there used to decay to zeros and still persist.
	#[test]
	fn a_feature_graph_shape_mismatch_aborts_instead_of_training_on_zeros() {
		let dim = 8;
		let mut snap = tiny_snapshot(6, dim);
		snap.features = Tensor::zeros(7, dim);
		let cfg = GnnConfig {
			train_epochs: DEFAULT_TRAIN_EPOCHS,
			..GnnConfig::defaults()
		};

		let err = match run_learned_propagation(&snap, &cfg) {
			Err(e) => e,
			Ok(_) => panic!("a shape mismatch must fail the whole propagation"),
		};
		assert!(
			err.starts_with("train epoch 0 forward:"),
			"the first epoch aborts, so one diagnostic is emitted, not one per matmul; got {err}"
		);
	}

	// The negative control for the seed (sources 1 and 2 of ROADMAP item 102).
	// Bit equality, not approximate: a tolerance would pass on two independently
	// trained models whose embeddings merely landed near each other. Restore
	// `rand::rng()` here and `n0` re-embeds 0.2173 against -0.1046. The snapshot
	// is built by hand, so the ORDER sources are controlled separately, by
	// `tick_gnn_propagate::two_identical_kerns_snapshot_in_the_same_order`.
	#[test]
	fn two_propagations_of_one_snapshot_are_bit_identical() {
		let dim = 8;
		let snap = tiny_snapshot(6, dim);
		let cfg = GnnConfig {
			train_epochs: 3,
			..GnnConfig::defaults()
		};

		let a = run_learned_propagation(&snap, &cfg).unwrap();
		let b = run_learned_propagation(&snap, &cfg).unwrap();

		// Embeddings before weights: both diverge together, and the embedding
		// diff is the one a human can read.
		for id in &snap.ids {
			let (va, vb) = (&a.updates[id], &b.updates[id]);
			let bits_a: Vec<u64> = va.iter().map(|x| x.to_bits()).collect();
			let bits_b: Vec<u64> = vb.iter().map(|x| x.to_bits()).collect();
			assert_eq!(
				bits_a, bits_b,
				"{id} re-embedded differently: {va:?} vs {vb:?}"
			);
		}
		assert_eq!(
			a.weights, b.weights,
			"the same snapshot must marshal the same weights"
		);
	}

	#[test]
	fn sample_negative_edges_avoids_positives_and_self_loops() {
		let pos = vec![[0, 1], [1, 2]];
		let mut rng = StdRng::seed_from_u64(5);
		let neg = sample_negative_edges(5, &pos, 4, &mut rng);
		for e in &neg {
			assert_ne!(e[0], e[1], "no self loops");
			let (lo, hi) = if e[0] < e[1] {
				(e[0], e[1])
			} else {
				(e[1], e[0])
			};
			assert!(
				!pos.contains(&[lo, hi]),
				"negative edge must not be a positive edge"
			);
		}
	}
}
mod reason_tests {
	use super::*;

	#[test]
	fn from_maps_every_field_without_drift() {
		let serde_cfg = config::GnnConfig {
			self_weight: 0.11,
			min_weight: 0.22,
			min_thoughts: 33,
			train_epochs: 44,
			train_learning_rate: 0.55,
		};
		let runtime: GnnConfig = serde_cfg.into();
		assert_eq!(runtime.self_weight, 0.11);
		assert_eq!(runtime.min_weight, 0.22);
		assert_eq!(runtime.min_thoughts, 33);
		assert_eq!(runtime.train_epochs, 44);
		assert_eq!(runtime.train_learning_rate, 0.55);
	}

	#[test]
	fn serde_default_equals_the_runtime_default() {
		let runtime: GnnConfig = config::GnnConfig::default().into();
		let rd = GnnConfig::defaults();
		assert_eq!(runtime.self_weight, rd.self_weight);
		assert_eq!(runtime.min_weight, rd.min_weight);
		assert_eq!(runtime.min_thoughts, rd.min_thoughts);
		assert_eq!(runtime.train_epochs, rd.train_epochs);
		assert_eq!(runtime.train_learning_rate, rd.train_learning_rate);
	}
}
