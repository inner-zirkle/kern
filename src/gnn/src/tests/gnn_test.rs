//! Tests extracted from gnn.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn relu_deriv_is_exact_at_and_near_kink() {
		assert_eq!(Activation::Relu.deriv(-2.0), 0.0);
		assert_eq!(Activation::Relu.deriv(-1e-6), 0.0);
		assert_eq!(Activation::Relu.deriv(0.0), 0.0);
		assert_eq!(Activation::Relu.deriv(1e-6), 1.0);
		assert_eq!(Activation::Relu.deriv(3.0), 1.0);
	}

	#[test]
	fn smooth_derivs_match_central_difference() {
		const H: f64 = 1e-6;
		let act = Activation::Sigmoid;
		for &x in &[-2.3, -0.5, 0.0, 0.7, 1.9] {
			let numeric = (act.forward(x + H) - act.forward(x - H)) / (2.0 * H);
			assert!(
				(act.deriv(x) - numeric).abs() < 1e-6,
				"{act:?} at {x}: analytic {} vs numeric {numeric}",
				act.deriv(x)
			);
		}
	}

	#[test]
	fn forward_dispatches_correctly() {
		assert_eq!(Activation::Relu.forward(-1.0), 0.0);
		assert_eq!(Activation::Relu.forward(2.0), 2.0);
		assert!((Activation::Sigmoid.forward(0.0) - 0.5).abs() < 1e-12);
	}
}
mod layer_tests {
	use super::*;
	use rand::rngs::StdRng;
	use rand::SeedableRng;

	fn layer(in_f: usize, out_f: usize) -> LinearLayer {
		let mut rng = StdRng::seed_from_u64(7);
		LinearLayer::with_rng(in_f, out_f, &mut rng)
	}

	#[test]
	fn forward_projects_to_out_features_width() {
		let mut l = layer(4, 3);
		let y = l.forward(&Tensor::zeros(2, 4));
		assert_eq!((y.rows, y.cols), (2, 3), "n_samples x out_features");
	}

	#[test]
	fn backward_dinput_shape_and_grad_accumulation() {
		let mut l = layer(4, 3);
		let x = Tensor::new(2, 4, vec![1.0; 8]).unwrap();
		let _ = l.forward(&x);
		let d_out = Tensor::new(2, 3, vec![1.0; 6]).unwrap();
		let d_in = l.backward(&d_out);

		assert_eq!((d_in.rows, d_in.cols), (2, 4), "dInput matches input shape");
		assert!(
			l.d_bias.data.iter().all(|&b| (b - 2.0).abs() < 1e-12),
			"d_bias = column sums of d_out"
		);
		assert!(
			l.d_weight.data.iter().all(|&w| (w - 2.0).abs() < 1e-12),
			"d_weight = Xᵀ·dOut"
		);
	}

	#[test]
	fn backward_accumulates_across_calls_until_zeroed() {
		let mut l = layer(2, 2);
		let x = Tensor::new(1, 2, vec![1.0, 1.0]).unwrap();
		let d_out = Tensor::new(1, 2, vec![1.0, 1.0]).unwrap();
		let _ = l.forward(&x);
		l.backward(&d_out);
		l.backward(&d_out);
		assert!(
			l.d_bias.data.iter().all(|&b| (b - 2.0).abs() < 1e-12),
			"two calls accumulate d_bias"
		);

		l.zero_grads();
		assert!(
			l.d_weight.data.iter().all(|&w| w == 0.0),
			"zero_grads clears d_weight in place"
		);
		assert!(l.d_bias.data.iter().all(|&b| b == 0.0));
	}

	#[test]
	fn forward_with_mismatched_input_width_zeroes_instead_of_panicking() {
		let mut l = layer(4, 3);
		let _ = l.forward(&Tensor::zeros(2, 4));

		let y = l.forward(&Tensor::zeros(2, 5));
		assert_eq!((y.rows, y.cols), (2, 3), "n_samples x out_features");
		assert!(y.data.iter().all(|&v| v == 0.0));
		assert!(matches!(
			l.try_backward(&Tensor::zeros(2, 3)).unwrap_err(),
			GnnError::MissingForwardState(_)
		));
	}

	#[test]
	fn try_backward_before_forward_is_a_missing_state_error() {
		let mut l = layer(2, 2);
		let d_out = Tensor::new(1, 2, vec![1.0, 1.0]).unwrap();
		assert!(matches!(
			l.try_backward(&d_out).unwrap_err(),
			GnnError::MissingForwardState(_)
		));

		let z = l.backward(&d_out);
		assert_eq!(
			(z.rows, z.cols),
			(1, 2),
			"fallback dInput is (n_samples, in_features)"
		);
		assert!(z.data.iter().all(|&v| v == 0.0));
	}
}
mod backward_tests {
	use super::*;

	#[test]
	fn relu_backward_is_exact_no_kink_bias() {
		let pre = Tensor {
			data: vec![-2.0, -1e-6, 0.0, 1e-6, 3.0],
			rows: 1,
			cols: 5,
		};
		let d_out = Tensor {
			data: vec![1.0; 5],
			rows: 1,
			cols: 5,
		};
		let g = act_deriv_mul(Activation::Relu, &d_out, &pre);
		assert_eq!(g.data, vec![0.0, 0.0, 0.0, 1.0, 1.0]);
	}
}
mod gnn_math_tests {
	use super::*;
	use crate::gnn_graph::Graph;
	use crate::gnn_tensor::Tensor;
	use rand::SeedableRng;

	fn tiny_graph() -> (Graph, Tensor) {
		let feats = [
			[0.5, -0.2, 0.1, 0.3],
			[-0.4, 0.6, 0.2, -0.1],
			[0.2, 0.1, -0.5, 0.4],
		];
		let mut g = Graph::new();
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

	fn assert_grad_matches_numeric(layer: &mut dyn BackwardGraphLayer, g: &Graph, x: &Tensor) {
		const H: f64 = 1e-6;
		let out = layer.forward_graph(g, x);
		let d_out = Tensor::ones(out.rows, out.cols);
		layer.zero_grads();
		layer.backward_graph(g, &d_out);
		let analytic: Vec<f64> = layer
			.param_grads()
			.iter()
			.flat_map(|t| t.data.clone())
			.collect();

		let lens: Vec<usize> = layer.parameters().iter().map(|t| t.data.len()).collect();
		let mut numeric = Vec::with_capacity(analytic.len());
		for (pi, &len) in lens.iter().enumerate() {
			for ei in 0..len {
				layer.parameters_mut()[pi].data[ei] += H;
				let lp = layer.forward_graph(g, x).sum_all();
				layer.parameters_mut()[pi].data[ei] -= 2.0 * H;
				let lm = layer.forward_graph(g, x).sum_all();
				layer.parameters_mut()[pi].data[ei] += H;
				numeric.push((lp - lm) / (2.0 * H));
			}
		}

		assert_eq!(analytic.len(), numeric.len(), "grad length mismatch");
		for (i, (a, n)) in analytic.iter().zip(&numeric).enumerate() {
			let denom = 1.0_f64.max(a.abs()).max(n.abs());
			assert!(
				(a - n).abs() / denom < 1e-4,
				"param grad[{i}]: analytic {a} vs numeric {n}"
			);
		}
	}

	fn assert_input_grad_matches_numeric(layer: &mut dyn BackwardGraphLayer, g: &Graph, x: &Tensor) {
		const H: f64 = 1e-6;
		let out = layer.forward_graph(g, x);
		let d_out = Tensor::ones(out.rows, out.cols);
		layer.zero_grads();
		let analytic = layer.backward_graph(g, &d_out);
		assert_eq!(
			analytic.shape(),
			x.shape(),
			"d_input shape must match features"
		);

		let mut numeric = Vec::with_capacity(x.data.len());
		for ei in 0..x.data.len() {
			let mut xp = x.clone();
			xp.data[ei] += H;
			let lp = layer.forward_graph(g, &xp).sum_all();
			let mut xm = x.clone();
			xm.data[ei] -= H;
			let lm = layer.forward_graph(g, &xm).sum_all();
			numeric.push((lp - lm) / (2.0 * H));
		}
		for (i, (a, n)) in analytic.data.iter().zip(&numeric).enumerate() {
			let denom = 1.0_f64.max(a.abs()).max(n.abs());
			assert!(
				(a - n).abs() / denom < 1e-4,
				"input grad[{i}]: analytic {a} vs numeric {n}"
			);
		}
	}

	#[test]
	fn gcn_linear_input_grad_matches_numeric() {
		let (g, x) = tiny_graph();
		let mut rng = rand::rngs::StdRng::seed_from_u64(23);
		let mut l = GCNLayer::with_rng(4, 3, None, false, &mut rng);
		assert_input_grad_matches_numeric(&mut l, &g, &x);
	}

	#[test]
	fn gcn_relu_input_grad_matches_numeric() {
		let (g, x) = tiny_graph();
		let mut rng = rand::rngs::StdRng::seed_from_u64(29);
		let mut l = GCNLayer::with_rng(4, 3, Some(Activation::Relu), false, &mut rng);
		assert_input_grad_matches_numeric(&mut l, &g, &x);
	}

	#[test]
	fn gcn_linear_backward_matches_numeric() {
		let (g, x) = tiny_graph();
		let mut rng = rand::rngs::StdRng::seed_from_u64(7);
		let mut l = GCNLayer::with_rng(4, 3, None, false, &mut rng);
		assert_grad_matches_numeric(&mut l, &g, &x);
	}

	#[test]
	fn gcn_relu_backward_matches_numeric() {
		let (g, x) = tiny_graph();
		let mut rng = rand::rngs::StdRng::seed_from_u64(11);
		let mut l = GCNLayer::with_rng(4, 3, Some(Activation::Relu), false, &mut rng);
		assert_grad_matches_numeric(&mut l, &g, &x);
	}

	#[test]
	fn matmul_and_transpose_are_correct() {
		let a = Tensor::new(2, 2, vec![1.0, 2.0, 3.0, 4.0]).unwrap();
		let b = Tensor::new(2, 2, vec![5.0, 6.0, 7.0, 8.0]).unwrap();
		let c = a.matmul(&b).unwrap();
		assert_eq!(c.data, vec![19.0, 22.0, 43.0, 50.0]);
		let at = a.transpose();
		assert_eq!(at.data, vec![1.0, 3.0, 2.0, 4.0]);
	}
}
mod gcn_tests {
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
mod norm_tests {
	use super::*;

	#[test]
	fn forward_normalizes_each_row() {
		let mut ln = LayerNorm::new(3);
		let x = Tensor::new(1, 3, vec![1.0, 2.0, 3.0]).unwrap();
		let out = ln.forward(&x);
		let mean: f64 = out.data.iter().sum::<f64>() / 3.0;
		assert!(mean.abs() < 1e-9, "row mean ~0, got {mean}");
		let var: f64 = out
			.data
			.iter()
			.map(|v| (v - mean) * (v - mean))
			.sum::<f64>()
			/ 3.0;
		assert!(
			(var - 1.0).abs() < 1e-3,
			"row var ~1 (minus epsilon), got {var}"
		);
	}

	#[test]
	fn parameters_returns_gamma_then_beta_in_order() {
		let ln = LayerNorm::new(3);
		let params = ln.parameters();
		assert_eq!(params.len(), 2);
		assert!(
			params[0].data.iter().all(|&v| v == 1.0),
			"param[0] is gamma (ones)"
		);
		assert!(
			params[1].data.iter().all(|&v| v == 0.0),
			"param[1] is beta (zeros)"
		);
		assert_eq!((params[0].rows, params[0].cols), (1, 3));
		assert_eq!((params[1].rows, params[1].cols), (1, 3));
		let grads = ln.param_grads();
		assert_eq!(grads.len(), 2);
		assert!(
			grads.iter().all(|g| g.data.iter().all(|&v| v == 0.0)),
			"fresh grads are zero"
		);
	}

	#[test]
	fn zero_grads_resets_accumulation_between_backward_passes() {
		let x = Tensor::new(1, 3, vec![1.0, 2.0, 3.0]).unwrap();
		let mut ln = LayerNorm::new(3);
		ln.forward(&x);
		let d_out = Tensor::ones(1, 3);

		ln.backward(&d_out);
		let after_one: Vec<f64> = ln.d_beta.data.clone();
		assert!(after_one.iter().all(|&v| (v - 1.0).abs() < 1e-12));

		ln.backward(&d_out);
		assert!(
			ln.d_beta.data.iter().all(|&v| (v - 2.0).abs() < 1e-12),
			"grads bleed across passes without a reset"
		);

		ln.zero_grads();
		assert!(
			ln.d_gamma.data.iter().all(|&v| v == 0.0),
			"zero_grads clears d_gamma"
		);
		assert!(
			ln.d_beta.data.iter().all(|&v| v == 0.0),
			"zero_grads clears d_beta"
		);

		ln.backward(&d_out);
		assert!(
			ln.d_beta
				.data
				.iter()
				.zip(&after_one)
				.all(|(now, one)| (now - one).abs() < 1e-12),
			"accumulation restarts from zero after zero_grads"
		);
	}

	#[test]
	fn try_backward_before_forward_is_a_missing_state_error() {
		let mut ln = LayerNorm::new(3);
		let d_out = Tensor::ones(1, 3);
		assert!(matches!(
			ln.try_backward(&d_out).unwrap_err(),
			GnnError::MissingForwardState(_)
		));
	}

	#[test]
	fn backward_before_forward_returns_zero_gradient_not_panic() {
		let mut ln = LayerNorm::new(3);
		let d_out = Tensor::ones(2, 3);
		let d_in = ln.backward(&d_out);
		assert_eq!(
			(d_in.rows, d_in.cols),
			(2, 3),
			"zero gradient matches d_out shape"
		);
		assert!(
			d_in.data.iter().all(|&v| v == 0.0),
			"missing forward state -> all-zero gradient"
		);
	}

	#[test]
	fn try_backward_equals_the_infallible_backward_after_forward() {
		let x = Tensor::new(2, 4, vec![0.5, -0.2, 0.1, 0.3, -0.4, 0.6, 0.2, -0.1]).unwrap();
		let d_out = Tensor::ones(2, 4);

		let mut a = LayerNorm::new(4);
		a.forward(&x);
		let via_try = a.try_backward(&d_out).expect("forward ran");

		let mut b = LayerNorm::new(4);
		b.forward(&x);
		let via_infallible = b.backward(&d_out);

		assert_eq!(
			via_try.data, via_infallible.data,
			"delegation preserves the gradient"
		);
	}

	#[test]
	fn backward_matches_numeric() {
		let x = Tensor::new(2, 4, vec![0.5, -0.2, 0.1, 0.3, -0.4, 0.6, 0.2, -0.1]).unwrap();
		const H: f64 = 1e-6;
		let mut ln = LayerNorm::new(4);
		let out = ln.forward(&x);
		let d_out = Tensor::ones(out.rows, out.cols);
		let d_in = ln.backward(&d_out); // loss = sum(output)

		for idx in 0..x.data.len() {
			let mut xp = x.clone();
			xp.data[idx] += H;
			let sp = LayerNorm::new(4).forward(&xp).sum_all();
			let mut xm = x.clone();
			xm.data[idx] -= H;
			let sm = LayerNorm::new(4).forward(&xm).sum_all();
			let num = (sp - sm) / (2.0 * H);
			let den = 1.0_f64.max(d_in.data[idx].abs()).max(num.abs());
			assert!(
				(d_in.data[idx] - num).abs() / den < 1e-4,
				"d_input[{idx}]: analytic {} vs numeric {num}",
				d_in.data[idx]
			);
		}
	}
}
mod loss_tests {
	use super::*;

	#[test]
	fn link_prediction_empty_edges_is_zero_loss_and_grad() {
		let emb = Tensor::new(3, 2, vec![1.0, 0.0, 0.0, 1.0, 1.0, 1.0]).unwrap();
		assert_eq!(link_prediction_loss(&emb, &[], &[]), 0.0);
		let g = link_prediction_grad(&emb, &[], &[]);
		assert_eq!((g.rows, g.cols), (3, 2));
		assert!(g.data.iter().all(|&v| v == 0.0));
	}

	#[test]
	fn link_prediction_aligned_positive_edge_has_lower_loss_than_opposed() {
		let aligned = Tensor::new(2, 2, vec![3.0, 0.0, 3.0, 0.0]).unwrap();
		let opposed = Tensor::new(2, 2, vec![3.0, 0.0, -3.0, 0.0]).unwrap();
		let pos = [[0usize, 1usize]];
		assert!(
			link_prediction_loss(&aligned, &pos, &[]) < link_prediction_loss(&opposed, &pos, &[]),
			"a positive edge between aligned embeddings is cheaper than between opposed ones"
		);
	}

	#[test]
	fn link_prediction_grad_matches_numerical_gradient() {
		let emb = Tensor::new(3, 2, vec![0.5, -0.2, 0.1, 0.3, -0.4, 0.6]).unwrap();
		let pos = [[0usize, 1usize], [1, 2]];
		let neg = [[0usize, 2usize]];
		let analytic = link_prediction_grad(&emb, &pos, &neg);
		const H: f64 = 1e-6;
		for idx in 0..emb.data.len() {
			let mut ep = emb.clone();
			ep.data[idx] += H;
			let mut em = emb.clone();
			em.data[idx] -= H;
			let num =
				(link_prediction_loss(&ep, &pos, &neg) - link_prediction_loss(&em, &pos, &neg)) / (2.0 * H);
			let den = 1.0_f64.max(analytic.data[idx].abs()).max(num.abs());
			assert!(
				(analytic.data[idx] - num).abs() / den < 1e-4,
				"grad[{idx}]: analytic {} vs numeric {num}",
				analytic.data[idx]
			);
		}
	}
}
mod optim_tests {
	use super::*;

	fn scalar(v: f64) -> Tensor {
		Tensor::new(1, 1, vec![v]).unwrap()
	}

	#[test]
	fn adam_first_step_is_lr_scaled_sign() {
		// At t=1 the bias-corrected update is lr * g / (|g| + eps) ~= lr*sign(g).
		let mut p = scalar(0.0);
		let g = scalar(2.0);
		let mut opt = Adam::new(0.1);
		opt.step(&mut [&mut p], &[&g]);
		assert!((p.data[0] - (-0.1)).abs() < 1e-6, "got {}", p.data[0]);
	}

	#[test]
	fn adam_keeps_independent_moment_state_per_parameter() {
		let mut p0 = scalar(0.0);
		let mut p1 = scalar(0.0);
		let g0 = scalar(2.0);
		let g1 = scalar(-2.0);
		let mut opt = Adam::new(0.1);
		opt.step(&mut [&mut p0, &mut p1], &[&g0, &g1]);
		assert!((p0.data[0] - (-0.1)).abs() < 1e-6, "p0 {}", p0.data[0]);
		assert!((p1.data[0] - 0.1).abs() < 1e-6, "p1 {}", p1.data[0]);
	}
}
mod model_tests {
	use super::*;
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
mod persist_tests {
	use super::*;
	use rand::rngs::StdRng;
	use rand::SeedableRng;

	fn small_model(seed: u64) -> Model {
		let mut rng = StdRng::seed_from_u64(seed);
		Model::new(vec![GCNLayer::with_rng(4, 3, None, false, &mut rng)], None)
	}

	#[test]
	fn marshal_unmarshal_round_trips_every_param_value_and_shape() {
		let src = small_model(1);
		let bytes = marshal_weights(&src).expect("marshal");

		let mut dst = small_model(999);
		unmarshal_weights(&mut dst, &bytes).expect("unmarshal");

		let sp = src.parameters();
		let dp = dst.parameters();
		assert_eq!(sp.len(), dp.len(), "parameter count preserved");
		assert!(
			!sp.is_empty(),
			"the model actually has parameters to compare"
		);
		for (a, b) in sp.iter().zip(&dp) {
			assert_eq!((a.rows, a.cols), (b.rows, b.cols), "shape preserved");
			assert_eq!(
				a.data, b.data,
				"every value is byte-identical after the round trip"
			);
		}
	}

	#[test]
	fn unmarshal_rejects_a_future_version_before_checking_params() {
		let wf = WeightFile {
			version: WEIGHT_FILE_VERSION + 1,
			params: Vec::new(),
		};
		let bytes = bincode::serde::encode_to_vec(&wf, bincode_cfg()).unwrap();
		let mut model = small_model(1);
		let err = unmarshal_weights(&mut model, &bytes).unwrap_err();
		assert!(
			matches!(err, PersistError::VersionMismatch { found, expected }
				if found == WEIGHT_FILE_VERSION + 1 && expected == WEIGHT_FILE_VERSION),
			"got {err:?}",
		);
	}

	#[test]
	fn unmarshal_rejects_a_corrupt_data_length_without_panicking() {
		let model = small_model(1);
		let records: Vec<TensorRecord> = model
			.parameters()
			.iter()
			.enumerate()
			.map(|(i, p)| TensorRecord {
				rows: p.rows,
				cols: p.cols,
				data: if i == 0 {
					p.data[..p.data.len() - 1].to_vec()
				} else {
					p.data.clone()
				},
			})
			.collect();
		let wf = WeightFile {
			version: WEIGHT_FILE_VERSION,
			params: records,
		};
		let bytes = bincode::serde::encode_to_vec(&wf, bincode_cfg()).unwrap();

		let mut dst = small_model(2);
		let err = unmarshal_weights(&mut dst, &bytes).unwrap_err();
		assert!(
			matches!(err, PersistError::DataLenMismatch { idx: 0, .. }),
			"corrupt data length must be a clean error, not a panic; got {err:?}"
		);
	}

	#[test]
	fn unmarshal_rejects_a_param_count_mismatch() {
		let wf = WeightFile {
			version: WEIGHT_FILE_VERSION,
			params: Vec::new(),
		};
		let bytes = bincode::serde::encode_to_vec(&wf, bincode_cfg()).unwrap();
		let mut model = small_model(1);
		let err = unmarshal_weights(&mut model, &bytes).unwrap_err();
		assert!(
			matches!(err, PersistError::CountMismatch { .. }),
			"got {err:?}"
		);
	}
}
