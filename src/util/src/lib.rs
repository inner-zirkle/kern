//! util — the leaf utilities nothing else depends on, that depend on nothing.
//!
//! Time (`now_nanos`, RFC-3339 parsing), content hashing, the log throttle, a
//! cheap profiler, and the file watcher. Every other kern crate may import
//! these; they import nothing from kern.
//!
//! Layer: L0 · May import: nothing (in kern).

pub mod profile;
pub mod util;
pub mod watcher;

pub use util::*;
