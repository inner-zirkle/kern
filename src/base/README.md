# base — the shared vocabulary every kern crate builds on

Layer: L1 · Status: **split**
May import: `util`
Absorbs: `src/base_constants.rs`, `src/base_types.rs`, `src/crdt.rs`

## What it owns

Entity identifiers, kinds (`EntityKind`, `Kern`, `Reason`, `Claim`), the
bitemporal timestamps, the `Entity` record itself, and the CRDT primitives
(`GCounter`) that reconcile entity state across federation. The tunable
constants (`base_constants`) that gate cold-tier GC, pulse decay, and heat
thresholds live here too — every layer reads them from one place.

## What it must never know

How an entity is stored (that is `store`), how it is retrieved (`retrieval`),
or how it travels over the wire (`gossip`). This is the type vocabulary; the
layers above give it behaviour.

## ABI

```rust
pub mod base_constants;
pub mod base_types;
pub mod crdt;

// base_types: Entity, EntityKind, EntityStatus, ReviewState, Embedding,
//   ChunkPart, ChunkPartKind, Kern, Reason, mk_entity, ...
// base_constants: COLD_GC_AGE, COLD_HEAT_THRESHOLD, PULSE_DECAY, PULSE_THRESHOLD, ...
// crdt: GCounter
```

## Invariants

- `Embedding` is an `Arc<[f32]>` — cheap to clone, immutable, shared.
- `mk_entity` is a non-`cfg(test)` constructor so downstream crates' tests can
  build entities without each re-stating the field list.
- A `GCounter` merge is commutative and associative; federation order must not
  matter.

## Tests

```
cargo test -p base
```
