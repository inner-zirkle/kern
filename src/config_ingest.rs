use serde::{Deserialize, Serialize};

use crate::base::constants::INGEST_DEDUP_THRESHOLD;
use crate::base::types::{EntityKind, Source};
use crate::ingest::ReviewPolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
	pub dedup_threshold: f64,
	/// Per-kind overrides indexed by `EntityKind as u8` (Fact=0 .. Conclusion=4).
	/// `None` falls back to `dedup_threshold`; default `[None; 5]` is
	/// bit-identical today. An operator can ask Facts to dedup tighter than
	/// Claims without tightening both (ROADMAP item 48 beside).
	#[serde(default = "default_dedup_threshold_by_kind")]
	pub dedup_threshold_by_kind: [Option<f64>; EntityKind::Conclusion as usize + 1],
	// Per-source-scheme curation policy, keyed on `Source::scheme()` — file,
	// ticket, session, agent, inline. An absent key is `active`, so the empty
	// default leaves every ingest retrievable exactly as before; `file =
	// "pending"` is how a host holds its watcher's auto-distilled claims out of
	// an `exclude_pending` query until `promote` curates them. Like
	// `source_trust` this weights the CHANNEL, not the author (ROADMAP 20).
	pub review_policy: ReviewPolicy,
}

fn default_dedup_threshold_by_kind() -> [Option<f64>; EntityKind::Conclusion as usize + 1] {
	[None; EntityKind::Conclusion as usize + 1]
}

impl Default for IngestConfig {
	fn default() -> Self {
		Self {
			dedup_threshold: INGEST_DEDUP_THRESHOLD,
			dedup_threshold_by_kind: default_dedup_threshold_by_kind(),
			review_policy: ReviewPolicy::new(),
		}
	}
}

impl IngestConfig {
	pub fn validate(&self) -> Result<(), String> {
		// A misspelled scheme would hold nothing back and read as a working knob,
		// so an unknown key is an error rather than a silent no-op.
		for scheme in self.review_policy.keys() {
			if Source::parse_scheme(scheme).is_none() {
				return Err(format!(
					"review_policy key {scheme:?} is not a source scheme (file, ticket, session, agent, inline)"
				));
			}
		}
		crate::ingest::Config {
			dedup_threshold: self.dedup_threshold,
			dedup_threshold_by_kind: self.dedup_threshold_by_kind,
			..Default::default()
		}
		.validate()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base::types::ReviewState;

	#[test]
	fn default_validates_and_bad_knobs_are_rejected() {
		assert!(
			IngestConfig::default().validate().is_ok(),
			"shipped defaults are valid"
		);

		let out_of_range = IngestConfig {
			dedup_threshold: 2.0,
			..Default::default()
		};
		assert!(
			out_of_range.validate().is_err(),
			"threshold outside [0,1] is rejected"
		);
	}

	#[test]
	fn an_unknown_review_policy_scheme_is_flagged() {
		let typo = IngestConfig {
			review_policy: ReviewPolicy::from([("files".to_string(), ReviewState::Pending)]),
			..Default::default()
		};
		assert!(
			typo.validate().unwrap_err().contains("review_policy"),
			"a scheme that names nothing must not read as a working policy"
		);

		let good = IngestConfig {
			review_policy: ReviewPolicy::from([("file".to_string(), ReviewState::Pending)]),
			..Default::default()
		};
		assert!(good.validate().is_ok(), "a real scheme is accepted");
	}
}
