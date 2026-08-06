use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ReloadConfig {
	// The daemon watches its own binary and hands the socket to a freshly
	// spawned successor when the file changes. Unix only; on Windows the
	// client-side auto-restart covers staleness instead.
	pub enabled: bool,
	pub poll_secs: u64,
}

impl Default for ReloadConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			poll_secs: 3,
		}
	}
}
