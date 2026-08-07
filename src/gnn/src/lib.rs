//! gnn — the small graph neural network that refines retrieval weights.
//!
//! A tiny propagate/train loop over the entity graph's adjacency, persisted as
//! a versioned weight file. The serde `config::GnnConfig` converts into the
//! runtime `gnn_propagate::GnnConfig` here (the conversion lives with the target
//! type, so `config` stays a leaf).
//!
//! Layer: L3 · May import: `config`.

pub mod gnn;
pub mod gnn_graph;
pub mod gnn_propagate;
pub mod gnn_tensor;

pub use gnn::*;
