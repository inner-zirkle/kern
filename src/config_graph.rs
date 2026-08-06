use serde::{Deserialize, Serialize};

use crate::base::constants::KERN_CAP_DISABLED;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
	pub max_kerns: usize,
	pub max_ledger_entries: usize,
	pub disk_threshold: usize,
}

impl Default for GraphConfig {
	fn default() -> Self {
		Self {
			// A conservative resident bound (ROADMAP item 83): most projects carry
			// <10 kerns. Eviction is proven safe — get_mut auto-loads, so the
			// post-register children-push lands on a reloaded copy that persists
			// (spawn_unnamed_child_under_cap_keeps_the_child_in_parent_children);
			// the old "drops unpersisted children pushes" comment was stale. 128
			// bounds the pathological case; eviction unloads to the cold tier, it
			// never forgets. disk_threshold stays disabled until item 75 (DiskANN
			// crash consistency) closes — arming it exposes the spill crash window.
			max_kerns: 128,
			max_ledger_entries: 10_000,
			disk_threshold: KERN_CAP_DISABLED,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_bounds_resident_kerns_conservatively() {
		// 128 is a safety bound, not a tuning knob: normal use is <10 kerns, and
		// eviction is proven safe (see GraphConfig::default). `usize::MAX` stays
		// the uncapped marker for an explicit opt-out.
		assert_eq!(GraphConfig::default().max_kerns, 128);
		assert_eq!(
			KERN_CAP_DISABLED,
			usize::MAX,
			"sentinel value is the uncapped marker"
		);
	}

	#[test]
	fn default_disables_disk_spill() {
		assert_eq!(GraphConfig::default().disk_threshold, KERN_CAP_DISABLED);
	}
}
