//! ingest — the pipeline that turns text into a bitemporal entity graph.
//!
//! Distill claims from raw text, embed them, dedup against the live graph,
//! place chunks under their document, and drive the worker that owns the
//! retry/timeout policy. The file watcher and intake queue feed it. Built on
//! `graph` (the graph it writes), `ingest_config` (policy), `llm` (embedder),
//! `base`/`math`/`util`.
//!
//! Layer: L4 · May import: `base`, `graph`, `ingest_config`, `llm`, `math`, `util`.

pub mod ingest;
pub mod ingest_dedup;
pub mod ingest_direct;
pub mod ingest_distill;
pub mod ingest_file_watcher;
pub mod ingest_filter;
pub mod ingest_intake;
pub mod ingest_intake_status;
pub mod ingest_place;
pub mod ingest_worker;

pub use ingest::*;
