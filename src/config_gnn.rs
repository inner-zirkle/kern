use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct GnnConfig {
	pub self_weight: f64,
	pub min_weight: f64,
	pub min_thoughts: usize,
	pub train_epochs: usize,
	pub train_learning_rate: f64,
}

impl Default for GnnConfig {
	fn default() -> Self {
		Self {
			self_weight: crate::gnn::propagate::DEFAULT_SELF_WEIGHT,
			min_weight: crate::gnn::propagate::DEFAULT_MIN_WEIGHT,
			min_thoughts: crate::gnn::propagate::DEFAULT_MIN_THOUGHTS,
			train_epochs: crate::gnn::propagate::DEFAULT_TRAIN_EPOCHS,
			train_learning_rate: crate::gnn::propagate::DEFAULT_TRAIN_LEARNING_RATE,
		}
	}
}

impl From<GnnConfig> for crate::gnn::propagate::GnnConfig {
	fn from(c: GnnConfig) -> Self {
		crate::gnn::propagate::GnnConfig {
			self_weight: c.self_weight,
			min_weight: c.min_weight,
			min_thoughts: c.min_thoughts,
			train_epochs: c.train_epochs,
			train_learning_rate: c.train_learning_rate,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn from_maps_every_field_without_drift() {
		let serde_cfg = GnnConfig {
			self_weight: 0.11,
			min_weight: 0.22,
			min_thoughts: 33,
			train_epochs: 44,
			train_learning_rate: 0.55,
		};
		let runtime: crate::gnn::propagate::GnnConfig = serde_cfg.into();
		assert_eq!(runtime.self_weight, 0.11);
		assert_eq!(runtime.min_weight, 0.22);
		assert_eq!(runtime.min_thoughts, 33);
		assert_eq!(runtime.train_epochs, 44);
		assert_eq!(runtime.train_learning_rate, 0.55);
	}

	#[test]
	fn serde_default_equals_the_runtime_default() {
		let runtime: crate::gnn::propagate::GnnConfig = GnnConfig::default().into();
		let rd = crate::gnn::propagate::GnnConfig::defaults();
		assert_eq!(runtime.self_weight, rd.self_weight);
		assert_eq!(runtime.min_weight, rd.min_weight);
		assert_eq!(runtime.min_thoughts, rd.min_thoughts);
		assert_eq!(runtime.train_epochs, rd.train_epochs);
		assert_eq!(runtime.train_learning_rate, rd.train_learning_rate);
	}
}
