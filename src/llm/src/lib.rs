//! llm — the embedder and reasoner client.
//!
//! A blocking-façade over an OpenAI-compatible HTTP endpoint: `embed`/`reason`
//! with retry, timeout, and a `LogThrottle` that collapses a repeating failure
//! to one line. Owns the canonical `DEFAULT_REASON_TIMEOUT_SECS` that `config`
//! reads down (so `config` never reaches back up to a runtime type).
//!
//! Layer: L2 · May import: `util`.

pub mod llm;
pub use llm::*;
