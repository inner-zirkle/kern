pub use crate::types::LlmFunc;
pub use crate::ingest_config::{review_for, valid_until_from_retention, Config, ReviewPolicy};
pub use crate::ingest_outcome::{FailureReport, Outcome, OutcomeStatus};
pub(crate) use crate::ingest_worker::Job;
pub use crate::ingest_worker::Worker;

pub use crate::ingest_config as config;
pub use crate::ingest_dedup as dedup;
pub use crate::ingest_direct as direct;
pub use crate::ingest_distill as distill;
pub use crate::ingest_embed as embed;
pub use crate::ingest_file_watcher as file_watcher;
pub use crate::ingest_intake as intake;
pub use crate::ingest_intake_status as intake_status;
pub use crate::ingest_outcome as outcome;
pub use crate::ingest_place as place;
pub use crate::ingest_split as split;
pub use crate::ingest_worker as worker;

#[cfg(test)]
pub(crate) fn stub_one_hot(seed: &str) -> Vec<f32> {
	let h = crate::util::content_hash(seed);
	let bytes = h.as_bytes();
	let slot = if bytes.is_empty() {
		0
	} else {
		bytes[0] as usize
	};
	let mut v = vec![0.0_f32; 256];
	v[slot] = 1.0;
	v
}
