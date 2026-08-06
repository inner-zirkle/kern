use serde::{Deserialize, Serialize};

// `dir` MUST stay cwd-relative and independent of `data_dir`:
// the MCP server resolves it from session cwd; deriving from data_dir breaks that contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IntakeConfig {
	pub enabled: bool,
	pub dir: String,
	pub poll_secs: u64,
	pub done_retention_secs: u64,
	// The TTL stamped on everything this queue ingests — "everything from this
	// source expires in 30 days", said once instead of on every call. Distinct
	// from `done_retention_secs`, which prunes the archived *files*: this one is
	// `valid_until` on the entity. 0 = no TTL, matching `--retention-secs`.
	// It lives here rather than in `[ingest]` because `Config::load_with_user`
	// refuses every tuning key in a user-written `[ingest]` — that table is
	// preset-owned, and its one exception is `review_policy`, which is curation
	// rather than tuning. A key no `kern.toml` can set is a key that ships dead.
	pub retention_secs: u64,
}

impl Default for IntakeConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			dir: ".kern/intake".into(),
			poll_secs: 5,
			done_retention_secs: 7 * 24 * 60 * 60,
			retention_secs: 0,
		}
	}
}

impl IntakeConfig {
	pub fn validate(&self) -> Result<(), String> {
		if self.enabled && self.poll_secs == 0 {
			return Err("poll_secs must be > 0 (0 busy-loops the intake drain)".into());
		}
		// Refuse a retention that can never become a deadline at boot, rather
		// than logging it once per drain pass for the life of the daemon.
		crate::ingest::valid_until_from_retention(self.retention_secs)?;
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn defaults_are_on_with_sane_tunables() {
		let c = IntakeConfig::default();
		assert!(c.enabled);
		assert_eq!(c.dir, ".kern/intake");
		assert_eq!(c.poll_secs, 5);
		assert_eq!(c.done_retention_secs, 604_800, "7 days in seconds");
		assert_eq!(c.retention_secs, 0, "no standing TTL unless a host asks");
	}

	#[test]
	fn validate_rejects_a_retention_that_can_never_become_a_deadline() {
		let unusable = IntakeConfig {
			retention_secs: u64::MAX,
			..Default::default()
		};
		assert!(
			unusable.validate().unwrap_err().contains("overflows"),
			"a retention no clock can represent is refused at load, not per drain"
		);

		let thirty_days = IntakeConfig {
			retention_secs: 30 * 24 * 60 * 60,
			..Default::default()
		};
		assert!(thirty_days.validate().is_ok(), "a real policy is accepted");
	}

	#[test]
	fn validate_rejects_zero_poll_only_when_enabled() {
		assert!(
			IntakeConfig::default().validate().is_ok(),
			"default (enabled, non-zero poll) is valid"
		);

		let zero_poll = IntakeConfig {
			enabled: true,
			poll_secs: 0,
			..Default::default()
		};
		assert!(zero_poll.validate().unwrap_err().contains("poll_secs"));

		let disabled_zero = IntakeConfig {
			enabled: false,
			poll_secs: 0,
			..Default::default()
		};
		assert!(
			disabled_zero.validate().is_ok(),
			"disabled intake ignores its poll interval"
		);
	}
}
