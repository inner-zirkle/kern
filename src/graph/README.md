# graph — the bitemporal entity graph and its indexes

Layer: L3 · Status: **split**
May import: `base`, `math`, `store`, `util`
Absorbs: `src/graph.rs`, `src/reason.rs`, `src/merge.rs`, `src/search.rs`,
`src/heat.rs`, `src/lexical.rs`, `src/accept.rs`, `src/persist.rs`, `src/diskann.rs`,
`src/hnsw.rs`, `src/vector_backend.rs`

## What it owns

`GraphGnn` — entities, reasons, and the vector indexes (HNSW, DiskANN, the
vector backend) that neighbour search walks. `accept` is the entity write
path; `merge` reconciles a remote entity in; `persist` serializes the graph;
`reason` adds reason edges; `search` and `lexical` are the primitive lookups
retrieval builds on; `heat` is the decay schedule.

## What it must never know

How retrieval scores, how the model trains, or how the graph federates. It
holds the graph and the indexes; the layers above give it policy.

## ABI

```rust
pub struct GraphGnn { /* entities, reasons, indexes, store binding */ }
pub mod accept;   // accept_entity, route_entity, graviton ops, seed_examples
pub mod graph;    // GraphGnn, all(), root, store(), quant_mode
pub mod merge;    // merge_remote_entity
pub mod reason;   // add_reason, collect_reason_ids
pub mod search;   // find_entity, find_reason, EntityHit
pub mod lexical;  // seed_lexical
pub mod heat;     // HeatConfig, decay
pub mod persist;  // save/load graph
pub mod hnsw;     // HNSW index
pub mod diskann;  // DiskANN index
pub mod vector_backend;  // the pluggable vector index backend
```

## Invariants

- diskann ↔ vector_backend reference each other; both live here so the cycle
  stays intra-crate.
- `accept` is the single entity write path; every other crate that adds an
  entity goes through it.
- `merge_remote_entity` is CRDT-ordered; federation arrival order must not
  change the final graph.

## Tests

```
cargo test -p graph
```
