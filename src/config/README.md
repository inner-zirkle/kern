# config — the serde configuration layer, a leaf every crate reads down to

Layer: L3 · Status: **split**
May import: `base`, `llm`, `ingest_config`, `util`
Absorbs: `src/config.rs`

## What it owns

The top-level `Config` and its per-subsystem sections (reason, embed, gnn,
ingest, intake, gossip, retrieval, ...), defaults, and validation that needs
only leaf types. Owns `HeatConfig` (the heat/decay knobs) and the canonical GNN
defaults (`DEFAULT_SELF_WEIGHT`, ...). Subsystem-runtime conversions live in
their own crates (`gnn::propagate::GnnConfig: From<config::GnnConfig>`; the
ingest policy in `ingest_config`) so this crate never reaches up to a runtime
type — that is the cycle break.

## What it must never know

A runtime config struct, the graph, the store, or the tick loop. It holds the
serde knobs and validates them with leaf types only.

## Tests

```
cargo test -p config
```
