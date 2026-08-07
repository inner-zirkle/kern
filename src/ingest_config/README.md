# ingest_config — ingest policy primitives, pure over base

Layer: L2 · Status: **split**
May import: `base`, `util`
Absorbs: `src/ingest_config.rs`

## What it owns

The runtime `Config` (dedup threshold, per-kind overrides, valid-from/until,
review policy), the `ReviewPolicy` type alias, `review_for`, and
`valid_until_from_retention`. Pure over `base` so both `config` (the serde
layer) and `ingest` (the runtime) depend down here instead of across each other.

## What it must never know

Storage, the graph, or the embedder. Policy math only.

## Tests

```
cargo test -p ingest_config
```
