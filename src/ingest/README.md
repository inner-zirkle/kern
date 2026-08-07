# ingest — the pipeline that turns text into a bitemporal entity graph

Layer: L4 · Status: **split**
May import: `base`, `graph`, `ingest_config`, `llm`, `math`, `util`
Absorbs: `src/ingest.rs`, `src/ingest_dedup.rs`, `src/ingest_direct.rs`,
`src/ingest_distill.rs`, `src/ingest_file_watcher.rs`, `src/ingest_intake.rs`,
`src/ingest_intake_status.rs`, `src/ingest_place.rs`, `src/ingest_worker.rs`

## What it owns

Distill claims from raw text, embed them, dedup against the live graph, place
chunks under their document, and the worker that owns retry/timeout policy.
The file watcher and the intake queue feed it. `ingest.rs` is the seam that
re-exports the stages as `dedup`/`direct`/`distill`/`file_watcher`/`intake`/
`intake_status`/`place`/`worker` and re-exports `ingest_config` as `config`.

## What it must never know

How retrieval scores, how the model trains, or that a tick loop schedules it.
It writes the graph; the loop above decides what to ingest.

## Tests

```
cargo test -p ingest
```
