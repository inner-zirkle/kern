use serde::{Deserialize, Serialize};

pub const DEFAULT_EMBED_URL: &str = "http://localhost:11434";
// Dimension-locked into the graph on first ingest: changing the model later
// requires `kern reembed` or stored vectors mismatch and search silently misses.
pub const DEFAULT_EMBED_MODEL: &str = "qwen3-embedding:0.6b";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedConfig {
	pub url: String,
	pub model: String,
	pub key: String,
	// Ollama-native only; ignored on `/v1` (warned at boot). 0 keeps the default.
	pub num_ctx: u64,
	// Ollama-native only; ignored on `/v1` (warned at boot). Empty keeps the default.
	pub keep_alive: String,
}

impl Default for EmbedConfig {
	fn default() -> Self {
		Self {
			url: DEFAULT_EMBED_URL.into(),
			model: DEFAULT_EMBED_MODEL.into(),
			key: String::new(),
			num_ctx: crate::llm::EMBED_NUM_CTX,
			keep_alive: crate::llm::EMBED_KEEP_ALIVE.into(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_uses_the_shared_constants() {
		let c = EmbedConfig::default();
		assert_eq!(c.url, DEFAULT_EMBED_URL);
		assert_eq!(c.model, DEFAULT_EMBED_MODEL);
		assert!(c.key.is_empty(), "no API key by default (local Ollama)");
	}
}
