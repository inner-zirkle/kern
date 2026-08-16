//! The ingest pipeline: turn raw text into graph entities.
//!
//! Text enters on one of three legs and lands in the graph through the same
//! [`Worker`] commit path, so the dedup, clamp, and retention gates apply
//! everywhere:
//!
//! - [`direct`] / [`intake`] — durable queue drops a host writes to disk; a
//!   background drain retries failed payloads forever rather than dropping them.
//! - [`file_watcher`] — `notify`-backed watch on file roots.
//! - the MCP tool's in-RAM queue via [`Worker::enqueue`] / [`Worker::submit`].
//!
//! The submodules are re-exported flat here; `crate::ingest::<area>` is the
//! stable spelling for the rest of the crate.

pub(crate) use crate::ingest_worker::Job;
pub use crate::ingest_worker::Worker;
pub use crate::ingest_worker::{FailureReport, Outcome, OutcomeStatus};
pub use ingest_config::{review_for, valid_until_from_retention, Config, ReviewPolicy};
pub use llm::LlmFunc;

pub use crate::ingest_dedup as dedup;
pub use crate::ingest_direct as direct;
pub use crate::ingest_distill as distill;
pub use crate::ingest_file_watcher as file_watcher;
pub use crate::ingest_intake as intake;
pub use crate::ingest_intake_status as intake_status;
pub use crate::ingest_place as place;
pub use crate::ingest_worker as worker;
pub use ingest_config as config;

#[cfg(test)]
#[path = "tests/ingest_test.rs"]
pub(crate) mod ingest_tests;
