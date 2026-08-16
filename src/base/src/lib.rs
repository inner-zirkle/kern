//! base — the shared vocabulary every kern crate builds on.
//!
//! Entity identifiers, kinds, the bitemporal timestamps, and the CRDT
//! primitives that reconcile them. Nothing here knows about
//! storage, the graph, or retrieval — every other crate stands on these.
//!
//! Layer: L0 · May import: nothing.

pub mod base_constants;
pub mod base_types;
pub mod crdt;
