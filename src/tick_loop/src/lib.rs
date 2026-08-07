//! tick_loop — the background loop that drains the task queue.
//!
//! [`start`] spawns the consumer that drains the `tick` queue one task at a time
//! against the shared graph: clustering, naming, enrichment, question seeding,
//! GNN propagation, GC. The orchestration that sits above the `tick`
//! primitives and every subsystem it drives.
//!
//! Layer: L6 · May import: `base`, `graph`, `store`, `retrieval`, `ingest`,
//!        `gnn`, `llm`, `config`, `tick`, `util`.

pub mod tick;
pub mod tick_cluster;
pub mod tick_gnn_propagate;
pub mod tick_idle;
pub mod tick_tasks;
pub mod tick_trainer;

pub use tick::*;
