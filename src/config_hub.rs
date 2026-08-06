use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
	// `kern mcp` spawns a detached machine-level hub when none answers, same as
	// it already auto-spawns a project daemon. false = hub is opt-in via
	// `kern hub`; the direct-connect fallback works either way.
	pub auto_start: bool,
	// A client attaching to a daemon built from a different binary, or booted
	// against a different config, restarts it before proxying. Without this a
	// long-lived daemon serves stale code and stale config indefinitely — the
	// failure that makes every shipped fix look like it did nothing.
	pub auto_restart: bool,
}

impl Default for HubConfig {
	fn default() -> Self {
		Self {
			auto_start: true,
			auto_restart: true,
		}
	}
}
