//! ingest_config — the ingest policy primitives, pure over `base`.
//!
//! The runtime `Config` (dedup threshold, per-kind overrides, valid-from/until,
//! review policy), the `ReviewPolicy` type alias, `review_for`, and
//! `valid_until_from_retention`. Both `config` (the serde layer) and `ingest`
//! (the runtime) depend down to this so neither reaches across the other.
//!
//! Layer: L2 · May import: `base`, `util`.

pub mod ingest_config;
pub use ingest_config::*;
