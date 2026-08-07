//! graph — the bitemporal entity graph and its indexes.
//!
//! `GraphGnn` holds entities, reasons, and the vector indexes (HNSW, DiskANN,
//! the vector backend) that neighbour search walks. `accept` is the entity
//! write path; `merge` reconciles a remote entity in; `persist` serializes the
//! graph; `reason` adds reason edges; `search` and `lexical` are the primitive
//! lookups retrieval builds on; `heat` is the decay schedule. Stands on `base`
//! (vocabulary), `math` (vectors), `store` (LMDB), `util`.
//!
//! Layer: L3 · May import: `base`, `math`, `store`, `util`.

pub mod accept;
pub mod diskann;
pub mod graph;
pub mod graph_ops;
pub mod heat;
pub mod hnsw;
pub mod lexical;
pub mod merge;
pub mod persist;
pub mod reason;
pub mod search;
pub mod vector_backend;

#[cfg(test)]
#[global_allocator]
static COUNTING: test_support::alloc_probe::Counting = test_support::alloc_probe::Counting;
