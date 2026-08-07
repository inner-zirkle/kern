# health — observable degradation counters

Layer: L4 · Status: **split**
Absorbs: `src/health.rs`

## What it owns

`HealthStats` — a single snapshot that names every silent failure mode in the
daemon: dropped embeddings, dimension rejected queries, cold-tier evictions,
queue refusals, delivery floors bypassed, clock-skew skips, remote-cap drops,
supersede-chain overflow, plus the Gini of access and kern-size for fairness
diagnosis. `graph_health_stats(g)` walks the graph and the cross-crate
process statics to assemble it; `gini_over_access` and `gini_over_kern_sizes`
are the pure-math helpers it uses.

## Must never know

The graph's write path, the model's training loop, or the polling cadence.
If a counter is hard to collect without holding a write lock, it's not a
health counter — it's a metrics hook that belongs in the subsystem.

## ABI

```rust
pub struct HealthStats { /* 20+ fields, see lib.rs */ }
pub fn graph_health_stats(g: &GraphGnn) -> HealthStats;
pub fn gini_over_access(counts: &[u64]) -> f64;
pub fn gini_over_kern_sizes(counts: &[usize]) -> f64;
```

## Invariants

- `gini_over_access(&[]) == 0.0` and equal counts → 0.0 (uniform = no spread).
- `f64` fields intentionally drop `Eq` — `HealthStats: PartialEq` only.
- `Default` is meaningful so a test can construct a counter without holding
  every process static.

## Tests

11 unit tests pin the empty-graph / storeless / supersede-chain / Gini
behaviour. No integration tests — health is read-only and observable.
