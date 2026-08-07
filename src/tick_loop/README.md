# tick_loop — the background loop that drains the task queue

Layer: L6 · Status: **split**
May import: `base`, `graph`, `store`, `retrieval`, `ingest`, `gnn`, `llm`,
`config`, `tick`, `util`
Absorbs: `src/tick.rs`, `src/tick_cluster.rs`, `src/tick_gnn_propagate.rs`,
`src/tick_idle.rs`, `src/tick_tasks.rs`, `src/tick_trainer.rs`

## What it owns

`start` spawns the consumer that drains the `tick` queue one task at a time
against the shared graph: clustering, naming, enrichment, question seeding, GNN
propagation (`tick_gnn_propagate`), the trainer (`tick_trainer`), idle sweep
(`tick_idle`), and the task dispatch (`tick_tasks`). This is the orchestration
above the `tick` primitives; it drives every subsystem so it depends on all of
them.

## What it must never know

Federation, the wire, or the CLI. It runs the loop; `gossip` federates, the
binary serves.

## Tests

```
cargo test -p tick_loop
```
