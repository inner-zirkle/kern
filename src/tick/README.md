# tick — scheduling primitives the loop and gossip build on

Layer: L4 · Status: **split**
May import: `base`, `graph`, `store`, `retrieval`, `config`, `util`
Absorbs: `src/tick_queue.rs`, `src/tick_pulse.rs`, `src/tick_stigmergy.rs`

## What it owns

`Queue` (seed-questions / classify-contradiction / re-embed tasks), `pulse`
(the periodic graph pulse that enqueues cluster work), and `stigmergy`
(access-heat decay + GC picking cold-tier victims). These are the primitives
`gossip` federates a pulse over and `loop` orchestrates — so they sit below
both, with no dep on either.

## What it must never know

The full tick loop, federation, or the model trainer. It enqueues and decays;
`loop` decides what to run.

## Tests

```
cargo test -p tick
```
