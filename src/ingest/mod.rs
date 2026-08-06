pub mod config;
pub mod dedup;
pub mod direct;
pub mod distill;
pub mod embed;
pub mod file_watcher;
pub mod intake;
pub mod intake_status;
pub mod outcome;
pub mod place;
pub mod split;
pub mod worker;

pub use crate::types::LlmFunc;
pub use config::{review_for, valid_until_from_retention, Config, ReviewPolicy};
pub use outcome::{FailureReport, Outcome, OutcomeStatus};
pub(crate) use worker::Job;
pub use worker::Worker;

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
