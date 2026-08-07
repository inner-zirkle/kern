//! The LLM/embed closure aliases shared by ingest, tick, and retrieval.

use std::sync::Arc;

/// Infallible by convention: an outage arrives as `""` — callers treat `""` as "skip".
pub type LlmFunc = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Fallible embedding call; the error string is surfaced to the caller's log.
pub type EmbedFunc = Arc<dyn Fn(&str) -> Result<Vec<f32>, String> + Send + Sync>;
