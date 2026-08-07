//! tick — the scheduling primitives the loop and gossip build on.
//!
//! `Queue` (the task queue: seed questions, classify contradictions, re-embed),
//! `pulse` (the periodic graph pulse that enqueues cluster work), and
//! `stigmergy` (the access-heat decay + GC that picks cold-tier victims). These
//! are the primitives `gossip` federates a pulse over and `loop` orchestrates;
//! they must stay below both.
//!
//! Layer: L4 · May import: `base`, `graph`, `store`, `retrieval`, `config`, `util`.

pub mod tick_pulse;
pub mod tick_queue;
pub mod tick_stigmergy;
