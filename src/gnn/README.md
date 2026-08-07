# gnn — the small graph neural network refining retrieval weights

Layer: L3 · Status: **split**
May import: `config`
Absorbs: `src/gnn.rs`, `src/gnn_graph.rs`, `src/gnn_propagate.rs`, `src/gnn_tensor.rs`

## What it owns

A tiny propagate/train loop over the entity graph's adjacency, persisted as a
versioned weight file (`WEIGHT_FILE_VERSION` — a bump is a wipe, never a
migration). `gnn_tensor` is the row-major matrix substrate; `gnn_graph` builds
the adjacency from the entity graph; `gnn_propagate` runs the forward/backward
pass and owns the runtime `GnnConfig` plus the `From<config::GnnConfig>`
conversion (the conversion lives with the target type so `config` stays a leaf).

## What it must never know

How retrieval scores, how the tick loop schedules it, or that anything
federates. It trains weights; the loop above decides when.

## Tests

```
cargo test -p gnn
```
