//! Propagation: run the trained model over a kern snapshot to produce the
//! GNN-refined embeddings the second ANN index serves, so structural
//! neighbours rank near each other even when raw content vectors differ.

use std::collections::{HashMap, HashSet};

use crate::gnn_activation::Activation;
use crate::gnn_gcn::GCNLayer;
use crate::gnn_graph::Graph;
use crate::gnn_loss::link_prediction_grad;
use crate::gnn_model::Model;
use crate::gnn_optim::Adam;
use crate::gnn_persist::{marshal_weights, unmarshal_weights};
use crate::gnn_tensor::Tensor;
use rand::rngs::StdRng;
use rand::SeedableRng;

/// Single source of truth for the GnnConfig defaults — both [`GnnConfig::defaults`]
/// and the serde `crate::config::GnnConfig` must read them from here, never re-literal.
pub const DEFAULT_SELF_WEIGHT: f64 = 0.6;
pub const DEFAULT_MIN_WEIGHT: f64 = 0.01;
pub const DEFAULT_MIN_THOUGHTS: usize = 128;
pub const DEFAULT_TRAIN_EPOCHS: usize = 24;
pub const DEFAULT_TRAIN_LEARNING_RATE: f64 = 0.01;

#[derive(Debug, Clone, Copy)]
pub struct GnnConfig {
	pub self_weight: f64,
	pub min_weight: f64,
	pub min_thoughts: usize,
	pub train_epochs: usize,
	pub train_learning_rate: f64,
}

impl GnnConfig {
	pub fn defaults() -> Self {
		Self {
			self_weight: DEFAULT_SELF_WEIGHT,
			min_weight: DEFAULT_MIN_WEIGHT,
			min_thoughts: DEFAULT_MIN_THOUGHTS,
			train_epochs: DEFAULT_TRAIN_EPOCHS,
			train_learning_rate: DEFAULT_TRAIN_LEARNING_RATE,
		}
	}
}

impl Default for GnnConfig {
	fn default() -> Self {
		Self::defaults()
	}
}

pub struct GnnSnapshot {
	pub ids: Vec<String>,
	pub features: Tensor,
	pub graph: Graph,
	pub pos_edges: Vec<[usize; 2]>,
	pub weights: Vec<u8>,
	/// Every draw this propagation makes — weight init and negative-edge
	/// sampling — comes from here, so one snapshot always trains to the same
	/// embeddings. Derived from the corpus by `tick_gnn_propagate::gnn_seed`;
	/// see there for why that input and not another (ROADMAP item 102).
	pub seed: u64,
}

pub struct PropagationResult {
	pub updates: HashMap<String, Vec<f64>>,
	pub weights: Vec<u8>,
}

pub fn run_learned_propagation(
	snap: &GnnSnapshot,
	cfg: &GnnConfig,
) -> Result<PropagationResult, String> {
	if snap.ids.is_empty() {
		return Err("empty snapshot".into());
	}
	let dim = snap.features.cols;
	let hidden = (dim / 2).clamp(16, 256);

	// One rng for the whole run, seeded off the snapshot: the negative set and
	// both layers' initial weights were the two unseeded `rand::rng()` draws that
	// made a propagation unrepeatable (ROADMAP item 102).
	let mut rng = StdRng::seed_from_u64(snap.seed);

	let neg_edges = sample_negative_edges(
		snap.ids.len(),
		&snap.pos_edges,
		snap.pos_edges.len(),
		&mut rng,
	);
	if neg_edges.is_empty() {
		return Err("could not sample negative edges".into());
	}

	let l1 = GCNLayer::with_rng(dim, hidden, Some(Activation::Relu), true, &mut rng);
	let l2 = GCNLayer::with_rng(hidden, dim, None, false, &mut rng);
	let mut model = Model::new(vec![l1, l2], None);

	if !snap.weights.is_empty() {
		if let Err(e) = unmarshal_weights(&mut model, &snap.weights) {
			tracing::error!(error = %e, "GNN weight load failed; cold-starting from fresh weights");
		}
	}

	let pos = snap.pos_edges.clone();
	let neg = neg_edges.clone();
	let mut optim = Adam::new(cfg.train_learning_rate);

	for epoch in 0..cfg.train_epochs {
		model.zero_grads();
		let predicted = model
			.forward(&snap.graph, &snap.features)
			.map_err(|e| format!("train epoch {epoch} forward: {e}"))?;
		let d_out = link_prediction_grad(&predicted, &pos, &neg);
		model
			.backward(&snap.graph, &d_out)
			.map_err(|e| format!("train epoch {epoch} backward: {e}"))?;

		let grads: Vec<Tensor> = model.param_grads().iter().map(|t| (*t).clone()).collect();
		let grad_refs: Vec<&Tensor> = grads.iter().collect();
		let mut params = model.parameters_mut();
		use crate::gnn_optim::Optimizer;
		optim.step(&mut params, &grad_refs);
	}

	let emb = model
		.forward(&snap.graph, &snap.features)
		.map_err(|e| format!("inference forward: {e}"))?;
	let mut updates = HashMap::new();

	for (i, id) in snap.ids.iter().enumerate() {
		let row = emb.row(i);
		if row.data.len() != dim {
			continue;
		}
		if has_nan_or_inf(&row.data) {
			continue;
		}
		let mut result = vec![0.0; dim];
		for (d, slot) in result.iter_mut().enumerate().take(dim) {
			*slot = cfg.self_weight * snap.features.at(i, d) + (1.0 - cfg.self_weight) * row.data[d];
		}
		updates.insert(id.clone(), gnn_normalize(&result));
	}

	let weights = marshal_weights(&model).map_err(|e| format!("marshal weights: {e}"))?;
	Ok(PropagationResult { updates, weights })
}

pub fn sample_negative_edges<R: rand::Rng>(
	n: usize,
	pos_edges: &[[usize; 2]],
	want: usize,
	rng: &mut R,
) -> Vec<[usize; 2]> {
	if n < 2 || want == 0 {
		return Vec::new();
	}
	let mut pos_set = HashSet::new();
	for e in pos_edges {
		let (a, b) = if e[0] < e[1] {
			(e[0], e[1])
		} else {
			(e[1], e[0])
		};
		pos_set.insert((a, b));
	}
	let max_pairs = n * (n - 1) / 2;
	let max_neg = max_pairs.saturating_sub(pos_set.len());
	if max_neg == 0 {
		return Vec::new();
	}
	let want = want.min(max_neg);

	use rand::RngExt;
	let mut neg_set = HashSet::new();
	let mut neg = Vec::with_capacity(want);
	let limit = want * 30;
	let mut attempts = 0;
	while neg.len() < want && attempts < limit {
		attempts += 1;
		let a = rng.random_range(0..n);
		let b = rng.random_range(0..n);
		if a == b {
			continue;
		}
		let (lo, hi) = if a < b { (a, b) } else { (b, a) };
		if pos_set.contains(&(lo, hi)) || neg_set.contains(&(lo, hi)) {
			continue;
		}
		neg_set.insert((lo, hi));
		neg.push([lo, hi]);
	}
	neg
}

pub fn gnn_normalize(v: &[f64]) -> Vec<f64> {
	let norm_sq: f64 = v.iter().map(|x| x * x).sum();
	if norm_sq == 0.0 {
		return v.to_vec();
	}
	let inv = 1.0 / norm_sq.sqrt();
	v.iter().map(|x| x * inv).collect()
}

fn has_nan_or_inf(v: &[f64]) -> bool {
	v.iter().any(|x| x.is_nan() || x.is_infinite())
}

#[cfg(test)]
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
