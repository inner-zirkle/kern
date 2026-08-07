# store — the LMDB storage primitive and its advisory lock

Layer: L2 · Status: **split**
May import: `base`, `math`, `util`
Absorbs: `src/base_store.rs`, `src/lock.rs`

## What it owns

`Store` — the environment, hot + cold entity tiers, the embedding index, and
the spill/GC bookkeeping that keeps a per-cwd graph bounded. `Lock` — the
cross-process advisory lock that makes a directory's store single-owner. The
`LogThrottle` on cold-eviction warns lives in `util`; the throttle instance is
per-store.

## What it must never know

What an entity means, how the graph walks, or that anything federates. It
holds bytes and counts evictions.

## ABI

```rust
pub struct Store { /* env, tiers, indices */ }
pub mod base_store;  // Store, StoreError, cold_spill, cold_cap, cold_evicted, ...
pub mod lock;        // Lock, the per-cwd advisory lock
```

## Invariants

- One store per directory, enforced by `Lock`. A second open of the same dir
  fails, not silently shares.
- `cold_cap` evicts oldest-first; the eviction counter accumulates and never
  resets. The cold-eviction warn is throttled to one line per
  `COLD_EVICT_WARN_SECS` window per store.
- bincode config is held at v2 — the encoded bytes ARE the persisted format; a
  bump is a `FORMAT_VERSION` wipe, not a routine update.

## Tests

```
cargo test -p store
```
