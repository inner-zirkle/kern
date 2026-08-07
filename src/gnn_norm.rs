//! Layer normalisation with learned scale/shift, and its backward pass.

use crate::gnn::GnnError;
use crate::gnn_layer::{Backward, Layer};
use crate::gnn_tensor::Tensor;

pub struct LayerNorm {
	pub gamma: Tensor, // 1×D
	pub beta: Tensor,  // 1×D
	pub epsilon: f64,
	pub dim: usize,
	pub last_x_hat: Option<Tensor>,
	last_inv_std: Vec<f64>,
	d_gamma: Tensor,
	d_beta: Tensor,
}

impl LayerNorm {
	pub fn new(dim: usize) -> Self {
		Self {
			gamma: Tensor::ones(1, dim),
			beta: Tensor::zeros(1, dim),
			epsilon: 1e-5,
			dim,
			last_x_hat: None,
			last_inv_std: Vec::new(),
			d_gamma: Tensor::zeros(1, dim),
			d_beta: Tensor::zeros(1, dim),
		}
	}

	pub fn try_backward(&mut self, d_out: &Tensor) -> Result<Tensor, GnnError> {
		let x_hat = self
			.last_x_hat
			.as_ref()
			.ok_or(GnnError::MissingForwardState("layernorm::last_x_hat"))?;
		let (n, d) = (d_out.rows, d_out.cols);
		let mut d_input = Tensor::zeros(n, d);

		for i in 0..n {
			let mut d_x_hat = vec![0.0; d];
			for (j, slot) in d_x_hat.iter_mut().enumerate().take(d) {
				*slot = d_out.at(i, j) * self.gamma.at(0, j);
				self.d_gamma.data[j] += d_out.at(i, j) * x_hat.at(i, j);
				self.d_beta.data[j] += d_out.at(i, j);
			}

			let mut sum_dx = 0.0;
			let mut sum_dx_xh = 0.0;
			for (j, &dxh) in d_x_hat.iter().enumerate().take(d) {
				sum_dx += dxh;
				sum_dx_xh += dxh * x_hat.at(i, j);
			}

			let scale = self.last_inv_std[i] / d as f64;
			for (j, &dxh) in d_x_hat.iter().enumerate().take(d) {
				d_input.set(
					i,
					j,
					scale * (d as f64 * dxh - sum_dx - x_hat.at(i, j) * sum_dx_xh),
				);
			}
		}
		Ok(d_input)
	}
}

impl Layer for LayerNorm {
	fn forward(&mut self, input: &Tensor) -> Tensor {
		let (n, d) = (input.rows, input.cols);
		let mut out = Tensor::zeros(n, d);
		let mut x_hat = Tensor::zeros(n, d);
		let mut inv_stds = vec![0.0; n];

		for (i, inv_std_slot) in inv_stds.iter_mut().enumerate().take(n) {
			let mut mean = 0.0;
			for j in 0..d {
				mean += input.at(i, j);
			}
			mean /= d as f64;

			let mut var = 0.0;
			for j in 0..d {
				let diff = input.at(i, j) - mean;
				var += diff * diff;
			}
			var /= d as f64;

			let inv_std = 1.0 / (var + self.epsilon).sqrt();
			*inv_std_slot = inv_std;

			for j in 0..d {
				let x = (input.at(i, j) - mean) * inv_std;
				x_hat.set(i, j, x);
				out.set(i, j, x * self.gamma.at(0, j) + self.beta.at(0, j));
			}
		}
		self.last_x_hat = Some(x_hat);
		self.last_inv_std = inv_stds;
		out
	}

	fn parameters(&self) -> Vec<&Tensor> {
		vec![&self.gamma, &self.beta]
	}

	fn parameters_mut(&mut self) -> Vec<&mut Tensor> {
		vec![&mut self.gamma, &mut self.beta]
	}
}

impl Backward for LayerNorm {
	fn backward(&mut self, d_out: &Tensor) -> Tensor {
		match self.try_backward(d_out) {
			Ok(t) => t,
			Err(e) => {
				tracing::error!(error = %e, "LayerNorm backward failed; returning zero gradient");
				Tensor::zeros(d_out.rows, d_out.cols)
			}
		}
	}

	fn param_grads(&self) -> Vec<&Tensor> {
		vec![&self.d_gamma, &self.d_beta]
	}

	fn param_grads_mut(&mut self) -> Vec<&mut Tensor> {
		vec![&mut self.d_gamma, &mut self.d_beta]
	}

	fn zero_grads(&mut self) {
		self.d_gamma = Tensor::zeros(1, self.dim);
		self.d_beta = Tensor::zeros(1, self.dim);
	}
}

#[cfg(test)]
mod tests {
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
