//! The graph-convolution layer: neighbourhood aggregation through the
//! normalised adjacency, with its forward and backward passes.

use crate::gnn::GnnError;
use crate::gnn_activation::Activation;
use crate::gnn_backward::{act_deriv_mul, BackwardGraphLayer, GraphLayer};
use crate::gnn_graph::Graph;
use crate::gnn_layer::{Backward, Layer, LinearLayer};
use crate::gnn_norm::LayerNorm;
use crate::gnn_sparse::SparseMatrix;
use crate::gnn_tensor::Tensor;

pub struct GCNLayer {
	pub linear: LinearLayer,
	pub norm: Option<LayerNorm>,
	pub act: Option<Activation>,
	last_norm_adj: Option<SparseMatrix>,
	last_pre_act: Option<Tensor>,
}

impl GCNLayer {
	pub fn new(in_features: usize, out_features: usize, act: Option<Activation>, norm: bool) -> Self {
		let mut rng = rand::rng();
		Self::with_rng(in_features, out_features, act, norm, &mut rng)
	}

	pub fn with_rng<R: rand::Rng>(
		in_features: usize,
		out_features: usize,
		act: Option<Activation>,
		norm: bool,
		rng: &mut R,
	) -> Self {
		Self {
			linear: LinearLayer::with_rng(in_features, out_features, rng),
			norm: if norm {
				Some(LayerNorm::new(out_features))
			} else {
				None
			},
			act,
			last_norm_adj: None,
			last_pre_act: None,
		}
	}

	pub fn try_forward_graph(&mut self, g: &Graph, features: &Tensor) -> Result<Tensor, GnnError> {
		let norm_adj = g.normalized_adjacency_sparse();
		let agg = norm_adj.matmul(features)?;
		self.last_norm_adj = Some(norm_adj);

		let mut h = self.linear.try_forward(&agg)?;
		if let Some(ref mut n) = self.norm {
			h = n.forward(&h);
		}
		self.last_pre_act = Some(h.clone());
		if let Some(a) = self.act {
			h = h.apply(|x| a.forward(x));
		}
		Ok(h)
	}

	pub fn try_backward_graph(&mut self, _g: &Graph, d_out: &Tensor) -> Result<Tensor, GnnError> {
		let norm_adj = self
			.last_norm_adj
			.as_ref()
			.ok_or(GnnError::MissingForwardState("gcn::last_norm_adj"))?
			.transpose();
		let mut grad = d_out.clone();
		if let Some(a) = self.act {
			let pre_act = self
				.last_pre_act
				.as_ref()
				.ok_or(GnnError::MissingForwardState("gcn::last_pre_act"))?;
			grad = act_deriv_mul(a, &grad, pre_act);
		}
		if let Some(ref mut n) = self.norm {
			grad = n.try_backward(&grad)?;
		}
		let d_agg = self.linear.try_backward(&grad)?;
		Ok(norm_adj.matmul(&d_agg)?)
	}
}

impl GraphLayer for GCNLayer {
	fn forward_graph(&mut self, g: &Graph, features: &Tensor) -> Tensor {
		match self.try_forward_graph(g, features) {
			Ok(t) => t,
			Err(e) => {
				tracing::debug!(error = %e, "GCNLayer forward_graph failed; returning zero activations");
				// Drop any stale cache so a later backward takes the MissingForwardState
				// path instead of multiplying against a shape this forward never produced.
				self.last_norm_adj = None;
				self.last_pre_act = None;
				Tensor::zeros(g.num_nodes(), self.linear.weight.cols)
			}
		}
	}

	fn parameters(&self) -> Vec<&Tensor> {
		let mut p = self.linear.parameters();
		if let Some(ref n) = self.norm {
			p.extend(Layer::parameters(n));
		}
		p
	}

	fn parameters_mut(&mut self) -> Vec<&mut Tensor> {
		let mut p = self.linear.parameters_mut();
		if let Some(ref mut n) = self.norm {
			p.extend(Layer::parameters_mut(n));
		}
		p
	}
}

impl BackwardGraphLayer for GCNLayer {
	fn backward_graph(&mut self, g: &Graph, d_out: &Tensor) -> Tensor {
		match self.try_backward_graph(g, d_out) {
			Ok(t) => t,
			Err(e) => {
				tracing::debug!(error = %e, "GCNLayer backward_graph failed; returning zero gradient");
				// dInput is (num_nodes, in_features); in_features == linear.weight.rows.
				Tensor::zeros(g.num_nodes(), self.linear.weight.rows)
			}
		}
	}

	fn param_grads(&self) -> Vec<&Tensor> {
		let mut g = self.linear.param_grads();
		if let Some(ref n) = self.norm {
			g.extend(Backward::param_grads(n));
		}
		g
	}

	fn param_grads_mut(&mut self) -> Vec<&mut Tensor> {
		let mut g = self.linear.param_grads_mut();
		if let Some(ref mut n) = self.norm {
			g.extend(Backward::param_grads_mut(n));
		}
		g
	}

	fn zero_grads(&mut self) {
		self.linear.zero_grads();
		if let Some(ref mut n) = self.norm {
			Backward::zero_grads(n);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rand::rngs::StdRng;
	use rand::SeedableRng;

	fn two_node_graph() -> Graph {
		let mut g = Graph::new();
		g.add_node("a", vec![1.0, 0.0]).unwrap();
		g.add_node("b", vec![0.0, 1.0]).unwrap();
		g.add_edge("a", "b").unwrap();
		g
	}

	#[test]
	fn forward_graph_aggregates_then_projects_to_out_features() {
		let g = two_node_graph();
		let feats = Tensor::new(2, 2, vec![1.0, 0.0, 0.0, 1.0]).unwrap();
		let mut rng = StdRng::seed_from_u64(1);
		let mut layer = GCNLayer::with_rng(2, 3, None, false, &mut rng);

		let out = layer.forward_graph(&g, &feats);
		assert_eq!((out.rows, out.cols), (2, 3), "num_nodes x out_features");
		let adj = layer
			.last_norm_adj
			.as_ref()
			.expect("normalized adjacency cached");
		assert_eq!(
			(adj.rows, adj.cols),
			(2, 2),
			"adjacency is num_nodes x num_nodes"
		);
		assert!(
			layer.last_pre_act.is_some(),
			"pre-activation cached for backward"
		);
	}

	#[test]
	fn forward_graph_with_mismatched_features_zeroes_instead_of_panicking() {
		let g = two_node_graph();
		let mut rng = StdRng::seed_from_u64(3);
		let mut layer = GCNLayer::with_rng(2, 3, None, false, &mut rng);
		let good = Tensor::new(2, 2, vec![1.0, 0.0, 0.0, 1.0]).unwrap();
		let _ = layer.forward_graph(&g, &good);

		// 3 rows against a 2-node adjacency: the aggregation cannot be formed.
		let bad = Tensor::zeros(3, 2);
		let out = layer.forward_graph(&g, &bad);
		assert_eq!((out.rows, out.cols), (2, 3), "num_nodes x out_features");
		assert!(out.data.iter().all(|&v| v == 0.0));
		assert!(matches!(
			layer
				.try_backward_graph(&g, &Tensor::ones(2, 3))
				.unwrap_err(),
			GnnError::MissingForwardState(_)
		));
	}

	#[test]
	fn try_backward_before_forward_is_missing_state_and_infallible_path_zeroes() {
		let g = two_node_graph();
		let mut rng = StdRng::seed_from_u64(2);
		let mut layer = GCNLayer::with_rng(2, 3, Some(Activation::Relu), false, &mut rng);
		let d_out = Tensor::ones(2, 3);

		assert!(matches!(
			layer.try_backward_graph(&g, &d_out).unwrap_err(),
			GnnError::MissingForwardState(_)
		));
		let z = layer.backward_graph(&g, &d_out);
		assert_eq!(
			(z.rows, z.cols),
			(2, 2),
			"fallback dInput is num_nodes x in_features"
		);
		assert!(z.data.iter().all(|&v| v == 0.0));
	}
}
