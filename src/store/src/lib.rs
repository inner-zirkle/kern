//! store — the per-process registry of open stores and the per-process
//! persist closure factory. `store_core` owns the LMDB environment;
//! `store::registry` owns the multi-dir bookkeeping the daemon uses to
//! dedupe concurrent `open()`s of the same dir.
//!
//! Layer: L4 · May import: `base`, `bootstrap`, `config`, `graph`, `ingest`,
//! `llm`, `store_core`, `tick`, `tick_loop`, `util`

pub mod registry;

pub use self::registry::{Registry, StoreEntry, StoreKey};

pub use store_core::lock;

pub use store_core::lock::WriterLock;
pub use store_core::Store;
