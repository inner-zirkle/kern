#[inline]
pub fn relu(x: f64) -> f64 {
	x.max(0.0)
}

#[inline]
pub fn relu_deriv(x: f64) -> f64 {
	if x > 0.0 {
		1.0
	} else {
		0.0
	}
}

#[inline]
pub fn sigmoid(x: f64) -> f64 {
	1.0 / (1.0 + (-x).exp())
}

#[inline]
pub fn sigmoid_deriv(x: f64) -> f64 {
	let s = sigmoid(x);
	s * (1.0 - s)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Activation {
	Relu,
	Sigmoid,
}

impl Activation {
	#[inline]
	pub fn forward(self, x: f64) -> f64 {
		match self {
			Activation::Relu => relu(x),
			Activation::Sigmoid => sigmoid(x),
		}
	}

	#[inline]
	pub fn deriv(self, x: f64) -> f64 {
		match self {
			Activation::Relu => relu_deriv(x),
			Activation::Sigmoid => sigmoid_deriv(x),
		}
	}
}

#[cfg(test)]
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
