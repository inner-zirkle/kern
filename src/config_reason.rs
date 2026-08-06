use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasonConfig {
	pub url: String,
	pub model: String,
	pub key: String,
	// Ceiling for one `complete` — the distill leg's slowest call. It was a
	// `const` nobody chose for this leg; the default is the number it was, so an
	// unconfigured kern posts under exactly the same bound. 0 keeps the default.
	pub timeout_secs: u64,
	// Ollama-native only; ignored on `/v1` (warned at boot). 0 keeps the default.
	pub num_ctx: u64,
	// Ollama-native only; ignored on `/v1` (warned at boot). Empty keeps the default.
	pub keep_alive: String,
}

const DEFAULT_REASON_URL: &str = "http://localhost:11434";

pub const DEFAULT_REASON_MODEL: &str = "granite4:3b";

// Slow CPU inference / large RAG prompts / long streams run past anything less.
pub const DEFAULT_REASON_TIMEOUT_SECS: u64 = 600;

impl Default for ReasonConfig {
	fn default() -> Self {
		Self {
			url: DEFAULT_REASON_URL.into(),
			model: DEFAULT_REASON_MODEL.into(),
			key: String::new(),
			timeout_secs: DEFAULT_REASON_TIMEOUT_SECS,
			num_ctx: crate::llm::REASON_NUM_CTX,
			keep_alive: crate::llm::REASON_KEEP_ALIVE.into(),
		}
	}
}
