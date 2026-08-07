//! Optimizers behind the [`Optimizer`] trait; Adam with bias correction is
//! the one training uses.

use crate::gnn_tensor::Tensor;

pub trait Optimizer {
	fn step(&mut self, params: &mut [&mut Tensor], grads: &[&Tensor]);
	fn zero_grad(&self, grads: &mut [Tensor]) {
		for g in grads.iter_mut() {
			for v in &mut g.data {
				*v = 0.0;
			}
		}
	}
}

pub struct Adam {
	pub lr: f64,
	pub beta1: f64,
	pub beta2: f64,
	pub epsilon: f64,
	step_count: usize,
	m: Vec<Tensor>,
	v: Vec<Tensor>,
}

impl Adam {
	pub fn new(lr: f64) -> Self {
		Self {
			lr,
			beta1: 0.9,
			beta2: 0.999,
			epsilon: 1e-8,
			step_count: 0,
			m: Vec::new(),
			v: Vec::new(),
		}
	}
}

impl Optimizer for Adam {
	fn step(&mut self, params: &mut [&mut Tensor], grads: &[&Tensor]) {
		if self.m.is_empty() {
			self.m = params
				.iter()
				.map(|p| Tensor::zeros(p.rows, p.cols))
				.collect();
			self.v = params
				.iter()
				.map(|p| Tensor::zeros(p.rows, p.cols))
				.collect();
		}
		self.step_count += 1;
		let t = self.step_count as f64;
		let bias_c1 = 1.0 - self.beta1.powf(t);
		let bias_c2 = 1.0 - self.beta2.powf(t);

		for (i, (param, grad)) in params.iter_mut().zip(grads.iter()).enumerate() {
			for j in 0..param.data.len() {
				let g = grad.data[j];
				self.m[i].data[j] = self.beta1 * self.m[i].data[j] + (1.0 - self.beta1) * g;
				self.v[i].data[j] = self.beta2 * self.v[i].data[j] + (1.0 - self.beta2) * g * g;

				let m_hat = self.m[i].data[j] / bias_c1;
				let v_hat = self.v[i].data[j] / bias_c2;

				param.data[j] -= self.lr * m_hat / (v_hat.sqrt() + self.epsilon);
			}
		}
	}
}

#[cfg(test)]
mod tests {
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
