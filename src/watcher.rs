//! File watching, re-exported flat: debounced notify events filtered through
//! gitignore-style rules into the pipeline sink `ingest_file_watcher` adapts.

pub use crate::watcher_event::{WatchEvent, WatchKind};
pub use crate::watcher_file::{FileWatcher, WatcherError};
pub use crate::watcher_ignore_rules::IgnoreRules;
pub use crate::watcher_pipeline::{IngestPipeline, IngestRecord, IngestSink, MAX_INGEST_BYTES};
