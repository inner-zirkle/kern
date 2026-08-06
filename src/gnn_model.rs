use crate::gnn_backward::{BackwardGraphLayer, GraphLayer};
use crate::gnn_gcn::GCNLayer;
use crate::gnn_graph::Graph;
use crate::gnn_layer::{Backward, Layer, LinearLayer};
use crate::gnn_tensor::Tensor;
use crate::gnn::GnnError;

// A `forward` must precede its `backward`; call `zero_grads` before each backward.
pub struct Model {
	pub layers: Vec<GCNLayer>,
	pub out_layer: Option<LinearLayer>,
}

impl Model {
	pub fn new(layers: Vec<GCNLayer>, out_layer: Option<LinearLayer>) -> Self {
		Self { layers, out_layer }
	}

	pub fn forward(&mut self, g: &Graph, features: &Tensor) -> Result<Tensor, GnnError> {
		let mut h = features.clone();
		for layer in &mut self.layers {
			h = layer.try_forward_graph(g, &h)?;
		}
		if let Some(ref mut ol) = self.out_layer {
			h = ol.try_forward(&h)?;
		}
		Ok(h)
	}

	pub fn backward(&mut self, g: &Graph, d_out: &Tensor) -> Result<(), GnnError> {
		let mut grad = d_out.clone();
		if let Some(ref mut ol) = self.out_layer {
			grad = ol.try_backward(&grad)?;
		}
		for layer in self.layers.iter_mut().rev() {
			grad = layer.try_backward_graph(g, &grad)?;
		}
		Ok(())
	}

	pub fn parameters(&self) -> Vec<&Tensor> {
		let mut p = Vec::new();
		for layer in &self.layers {
			p.extend(GraphLayer::parameters(layer));
		}
		if let Some(ref ol) = self.out_layer {
			p.extend(Layer::parameters(ol));
		}
		p
	}

	pub fn parameters_mut(&mut self) -> Vec<&mut Tensor> {
		let mut p = Vec::new();
		for layer in &mut self.layers {
			p.extend(GraphLayer::parameters_mut(layer));
		}
		if let Some(ref mut ol) = self.out_layer {
			p.extend(Layer::parameters_mut(ol));
		}
		p
	}

	pub fn param_grads(&self) -> Vec<&Tensor> {
		let mut g = Vec::new();
		for layer in &self.layers {
			g.extend(layer.param_grads());
		}
		if let Some(ref ol) = self.out_layer {
			g.extend(Backward::param_grads(ol));
		}
		g
	}

	pub fn param_grads_mut(&mut self) -> Vec<&mut Tensor> {
		let mut g = Vec::new();
		for layer in &mut self.layers {
			g.extend(layer.param_grads_mut());
		}
		if let Some(ref mut ol) = self.out_layer {
			g.extend(Backward::param_grads_mut(ol));
		}
		g
	}

	pub fn zero_grads(&mut self) {
		for layer in &mut self.layers {
			layer.zero_grads();
		}
		if let Some(ref mut ol) = self.out_layer {
			Backward::zero_grads(ol);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::gnn_gcn::GCNLayer;
	use rand::rngs::StdRng;
	use rand::SeedableRng;

	fn tiny_graph() -> (Graph, Tensor) {
		let mut g = Graph::new();
		let feats = [
			[0.5, -0.2, 0.1, 0.3],
			[-0.4, 0.6, 0.2, -0.1],
			[0.2, 0.1, -0.5, 0.4],
		];
		for (i, f) in feats.iter().enumerate() {
			g.add_node(&format!("n{i}"), f.to_vec()).unwrap();
		}
		g.add_edge("n0", "n1").unwrap();
		g.add_edge("n1", "n2").unwrap();
		g.add_edge("n2", "n0").unwrap();
		g.add_self_loops();
		let x = g.feature_matrix();
		(g, x)
	}

	fn one_layer_model(in_f: usize, out_f: usize, seed: u64) -> Model {
		let mut rng = StdRng::seed_from_u64(seed);
		Model::new(
			vec![GCNLayer::with_rng(in_f, out_f, None, false, &mut rng)],
			None,
		)
	}

	#[test]
	fn forward_projects_to_out_layer_width_and_is_finite() {
		let (g, x) = tiny_graph();
		let mut model = one_layer_model(4, 3, 3);
		let out = model.forward(&g, &x).expect("shapes agree");
		assert_eq!(out.rows, g.num_nodes(), "one row per node");
		assert_eq!(out.cols, 3, "width equals the layer's out_features");
		assert!(out.data.iter().all(|v| v.is_finite()), "no NaN/inf");
	}

	#[test]
	fn forward_surfaces_an_aggregation_mismatch_instead_of_zeroing() {
		let (g, _) = tiny_graph();
		let mut model = one_layer_model(4, 3, 5);
		// 5 feature rows against a 3-node adjacency: aggregation cannot be formed.
		let err = model
			.forward(&g, &Tensor::zeros(5, 4))
			.expect_err("a mismatch must reach the caller, not decay to zeros");
		assert!(matches!(err, GnnError::Tensor(_)), "got {err:?}");
	}

	// The projection stage is the one that used to swallow: `Layer::forward` logs
	// and returns zeros, so the whole run reported success on garbage.
	#[test]
	fn a_projection_mismatch_fails_the_forward_and_then_the_backward() {
		let (g, _) = tiny_graph();
		let mut model = one_layer_model(4, 3, 9);
		// Aggregation succeeds (3 nodes, 3 rows); the 2-wide result cannot enter a
		// linear layer expecting 4 inputs.
		let err = model
			.forward(&g, &Tensor::zeros(3, 2))
			.expect_err("the projection mismatch must reach the caller");
		assert!(matches!(err, GnnError::Tensor(_)), "got {err:?}");

		let err = model
			.backward(&g, &Tensor::ones(3, 3))
			.expect_err("no forward completed, so no gradient can be honest");
		assert!(
			matches!(err, GnnError::MissingForwardState(_)),
			"got {err:?}"
		);
	}

	#[test]
	fn backward_without_a_forward_is_an_error_not_a_zero_gradient() {
		let (g, _) = tiny_graph();
		let mut model = one_layer_model(4, 3, 7);
		let err = model
			.backward(&g, &Tensor::ones(3, 3))
			.expect_err("no cached forward state -> error");
		assert!(
			matches!(err, GnnError::MissingForwardState(_)),
			"got {err:?}"
		);
		assert!(
			model
				.param_grads()
				.iter()
				.all(|t| t.data.iter().all(|&v| v == 0.0)),
			"a rejected backward accumulates no gradient"
		);
	}
}
