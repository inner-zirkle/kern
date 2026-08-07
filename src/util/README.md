# util — the leaf nothing depends down to

Layer: L0 · Status: **split**
May import: nothing (in kern)
Absorbs: `src/util.rs`, `src/profile.rs`, `src/watcher.rs`

## What it owns

The small primitives every other kern crate reaches for and none of them
should duplicate: nanosecond time (`now_nanos`), RFC-3339 parsing, content
hashing, the `LogThrottle` that deduplicates a repeated warn, a cheap
allocation profiler, and the file watcher that drives intake.

## What it must never know

What an entity is, what a graph is, that anything is being stored. This is the
floor; anything domain-shaped pushed down here creates the cycle the split
exists to break.

## ABI

```rust
pub fn now_nanos() -> i128;
pub fn content_hash(s: &str) -> String;
pub fn parse_rfc3339(s: &str) -> Result<std::time::SystemTime, ()>;
pub fn date_string(now: std::time::SystemTime) -> String;
pub struct LogThrottle { /* ... */ }
pub mod profile;
pub mod watcher;
```

## Invariants

- `content_hash` is a stable hex digest; persisted IDs depend on it not drifting.
- `LogThrottle::allow` is process-global per key; tests that assert a single
  warn must run on a fresh store (the throttle is per-store, not per-process).

## Tests

```
cargo test -p util
```
