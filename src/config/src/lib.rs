//! config — the serde configuration layer, a leaf every crate reads down to.
//!
//! The top-level `Config` and its per-subsystem sections (reason, embed, gnn,
//! ingest, intake, retrieval, ...), defaults, and validation that needs
//! only leaf types. Subsystem-runtime conversions live in their own crates
//! (`gnn::propagate::GnnConfig: From<config::GnnConfig>`, the ingest policy in
//! `ingest_config`) so this crate never reaches up to a runtime type.
//!
//! Layer: L3 · May import: `base`, `llm`, `ingest_config`, `util`.

pub mod config;
pub use config::*;
