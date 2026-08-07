//! store — the per-process registry of open stores. One daemon serves many
//! data dirs; each `StoreEntry` bundles a dir's graph, ingest worker, tick
//! queue, and the single persist closure, keyed by canonical path so two
//! callers naming the same dir share one instance (LMDB forbids a double-open).

pub use store_core::lock;

pub use store_core::Store;
pub use store_core::lock::WriterLock;
