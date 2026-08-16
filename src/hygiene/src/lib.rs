//! hygiene — deterministic noise and secret detection, pure over text.
//!
//! The one scoring core behind the write-time gate (`ingest`) and the stored-
//! content audit (`commands`, `mcp`): noise patterns, labelled secret patterns,
//! the non-additive noise score, and the action suggestion. No LLM, no
//! embeddings, no I/O — regex and arithmetic, so it runs anywhere and produces
//! the same answer twice. Ported from mnemosyne's hygiene/filters subsystem
//! (MIT), adapted to kern's entity model.
//!
//! Layer: L1 · May import: nothing of ours.

pub mod hygiene;
pub use hygiene::*;
