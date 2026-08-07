//! store — the LMDB-backed storage primitive and its advisory lock.
//!
//! `Store` owns the environment, the hot/cold tiers, the embedding index, and
//! the spill/GC bookkeeping. `Lock` is the cross-process advisory lock that
//! makes a per-cwd store single-owner. Stands on `base` (entity vocabulary) and
//! `math` (quantized embeddings); nothing above it knows LMDB.
//!
//! Layer: L2 · May import: `base`, `math`, `util`.

pub mod base_store;
pub mod lock;

pub use base_store::*;
