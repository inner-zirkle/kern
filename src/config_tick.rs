use serde::{Deserialize, Serialize};

use crate::base::constants::{
	KERN_IDLE_TIMEOUT, TICK_INTERVAL_SECS, TICK_MAX_CLUSTER_SAMPLE, TICK_QUEUE_CAPACITY,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct TickConfig {
	pub max_cluster_sample: usize,
	pub queue_capacity: usize,
	// `0` disables the driver: an idle daemon never decays heat or evicts cold nodes.
	pub interval_secs: u64,
	// `0` keeps every loaded kern resident forever.
	pub kern_idle_timeout_secs: u64,
}

impl Default for TickConfig {
	fn default() -> Self {
		Self {
			max_cluster_sample: TICK_MAX_CLUSTER_SAMPLE,
			queue_capacity: TICK_QUEUE_CAPACITY,
			interval_secs: TICK_INTERVAL_SECS,
			kern_idle_timeout_secs: KERN_IDLE_TIMEOUT.as_secs(),
		}
	}
}
