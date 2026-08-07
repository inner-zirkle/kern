//! Ingest policy knobs: dedup threshold, clamp lengths, retention-to-TTL
//! mapping, and the review policy that decides which sources land `pending`
//! (held for curation) versus `active`.

use crate::base_constants::INGEST_DEDUP_THRESHOLD;
use crate::base_types::{EntityKind, ReviewState, Source};

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

/// Source scheme → the curation state a claim arriving on it is placed in. An
/// absent key is `Active`, so an empty policy is today's behaviour exactly.
pub type ReviewPolicy = BTreeMap<String, ReviewState>;

/// The one resolution, so no producer can key on something other than the
/// scheme `IngestConfig::validate` checks against.
pub fn review_for(policy: &ReviewPolicy, source: &Source) -> ReviewState {
	policy.get(source.scheme()).copied().unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct Config {
	pub dedup_threshold: f64,
	/// Per-kind overrides indexed by `EntityKind as u8` (Fact=0 .. Conclusion=4).
	/// `None` falls back to `dedup_threshold`. Default `[None; 5]` is
	/// bit-identical to a single global threshold — an operator can ask Facts
	/// to dedup tighter than Claims without tightening both (ROADMAP item 48
	/// beside). Preset-owned, not auto-tuned.
	pub dedup_threshold_by_kind: [Option<f64>; EntityKind::Conclusion as usize + 1],
	pub valid_from: Option<std::time::SystemTime>,
	pub valid_until: Option<std::time::SystemTime>,
	// The POLICY, not a resolved state: the intake drain hands one `Config` to a
	// whole pass of records whose sources differ, so the scheme is only known
	// per job. `job()` resolves it — the single gate every producer passes.
	pub review_policy: ReviewPolicy,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			dedup_threshold: INGEST_DEDUP_THRESHOLD,
			dedup_threshold_by_kind: [None; EntityKind::Conclusion as usize + 1],
			valid_from: None,
			valid_until: None,
			review_policy: ReviewPolicy::new(),
		}
	}
}

// Retention is a duration at the caller boundary and an absolute instant on the
// entity. The single conversion lives here so the CLI flag and the MCP field
// cannot drift apart; 0 means "no TTL", matching every other unset knob.
pub fn valid_until_from_retention(retention_secs: u64) -> Result<Option<SystemTime>, String> {
	if retention_secs == 0 {
		return Ok(None);
	}
	SystemTime::now()
		.checked_add(Duration::from_secs(retention_secs))
		.map(Some)
		.ok_or_else(|| format!("retention_secs {retention_secs} overflows the clock"))
}

impl Config {
	/// Per-kind dedup threshold: a `Some` override on the kind's slot wins, else
	/// the global `dedup_threshold`. Indexed by `EntityKind as u8` so it is O(1)
	/// and needs no `Hash` derive on `EntityKind`.
	pub fn dedup_threshold_for(&self, kind: EntityKind) -> f64 {
		self.dedup_threshold_by_kind[kind as usize].unwrap_or(self.dedup_threshold)
	}

	/// The same conversion, resolved *now*, for the entrances whose retention is
	/// a standing policy rather than a per-call argument. Long-lived callers (the
	/// intake poll loop, the file watcher) must build one per pass: resolving it
	/// once at startup would stamp a file seen on day 30 with a deadline measured
	/// from boot. `Config::validate` refuses an unrepresentable retention at
	/// load, so an error here is a caller that skipped it — say so, then no TTL.
	pub fn with_retention(mut self, retention_secs: u64) -> Self {
		self.valid_until = valid_until_from_retention(retention_secs).unwrap_or_else(|e| {
			tracing::error!(target: "kern.ingest", error = %e, "unusable retention_secs; ingesting with no TTL");
			None
		});
		self
	}

	pub fn validate(&self) -> Result<(), String> {
		if !(0.0..=1.0).contains(&self.dedup_threshold) {
			return Err(format!(
				"dedup_threshold must be in [0.0, 1.0], got {}",
				self.dedup_threshold
			));
		}
		for (i, slot) in self.dedup_threshold_by_kind.iter().enumerate() {
			if let Some(t) = slot {
				if !(0.0..=1.0).contains(t) {
					let kind = EntityKind::from_u8(i as u8)
						.map(|k| k.as_str())
						.unwrap_or("unknown");
					return Err(format!(
						"dedup_threshold_by_kind[{kind}] must be in [0.0, 1.0], got {t}"
					));
				}
			}
		}
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base_types::EntityKind;

	#[test]
	fn runtime_and_serde_ingest_defaults_agree() {
		let rt = Config::default();
		let serde = crate::config::IngestConfig::default();
		assert_eq!(rt.dedup_threshold, serde.dedup_threshold);
		assert_eq!(rt.dedup_threshold, INGEST_DEDUP_THRESHOLD);
		assert_eq!(rt.review_policy, serde.review_policy);
		assert!(
			rt.review_policy.is_empty(),
			"nothing is held for review until a host asks"
		);
	}

	#[test]
	fn review_for_keys_on_the_scheme_and_defaults_to_active() {
		let file = Source::File {
			path: "/a".into(),
			section: String::new(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		};
		let inline = Source::Inline {
			hash: "h".into(),
			section: String::new(),
		};
		let policy = ReviewPolicy::from([("file".to_string(), ReviewState::Pending)]);
		assert_eq!(review_for(&policy, &file), ReviewState::Pending);
		assert_eq!(
			review_for(&policy, &inline),
			ReviewState::Active,
			"an unlisted scheme is active — the policy holds back only what it names"
		);
		assert_eq!(
			review_for(&ReviewPolicy::new(), &file),
			ReviewState::Active,
			"an empty policy holds nothing back"
		);
	}

	#[test]
	fn validate_accepts_the_default_and_rejects_bad_knobs() {
		assert!(
			Config::default().validate().is_ok(),
			"default config is valid"
		);

		let out_of_range = Config {
			dedup_threshold: 1.5,
			..Default::default()
		};
		assert!(out_of_range
			.validate()
			.unwrap_err()
			.contains("dedup_threshold"));
	}

	#[test]
	fn retention_becomes_an_absolute_deadline_one_hour_out() {
		let before = SystemTime::now();
		let got = valid_until_from_retention(3600)
			.expect("an hour is representable")
			.expect("a non-zero retention yields a deadline");
		let after = SystemTime::now();
		assert!(
			got >= before + Duration::from_secs(3600) && got <= after + Duration::from_secs(3600),
			"valid_until is now + 1h"
		);
	}

	#[test]
	fn omitted_retention_leaves_no_deadline() {
		assert_eq!(
			valid_until_from_retention(0).expect("zero is not an error"),
			None,
			"0 means no TTL"
		);
		assert_eq!(
			Config::default().valid_until,
			None,
			"a default ingest sets no valid_until"
		);
	}

	#[test]
	fn with_retention_carries_a_standing_policy_onto_the_config() {
		let before = SystemTime::now();
		let cfg = Config {
			dedup_threshold: 0.9,
			..Default::default()
		}
		.with_retention(3600);
		let got = cfg
			.valid_until
			.expect("a policy retention yields a deadline");
		assert!(
			got >= before + Duration::from_secs(3600),
			"the deadline is resolved at call time, not at startup"
		);
		assert_eq!(cfg.dedup_threshold, 0.9, "the other knobs survive");

		assert_eq!(
			Config::default().with_retention(0).valid_until,
			None,
			"no configured policy means no TTL"
		);
	}

	#[test]
	fn retention_that_overflows_the_clock_is_rejected_loudly() {
		assert!(valid_until_from_retention(u64::MAX)
			.unwrap_err()
			.contains("overflows the clock"));
	}

	#[test]
	fn dedup_threshold_for_kind_resolves() {
		let mut cfg = Config::default();
		// None -> global.
		assert_eq!(
			cfg.dedup_threshold_for(EntityKind::Fact),
			cfg.dedup_threshold
		);
		assert_eq!(
			cfg.dedup_threshold_for(EntityKind::Claim),
			cfg.dedup_threshold,
			"a None slot falls back to the global threshold"
		);
		// Some -> override wins.
		cfg.dedup_threshold_by_kind[EntityKind::Fact as usize] = Some(0.99);
		cfg.dedup_threshold_by_kind[EntityKind::Question as usize] = Some(0.80);
		assert_eq!(cfg.dedup_threshold_for(EntityKind::Fact), 0.99);
		assert_eq!(cfg.dedup_threshold_for(EntityKind::Question), 0.80);
		// A None slot between two Some slots still falls back.
		assert_eq!(
			cfg.dedup_threshold_for(EntityKind::Claim),
			cfg.dedup_threshold,
			"a None slot is unaffected by a neighbour's Some"
		);
	}

	#[test]
	fn validate_rejects_out_of_range_per_kind() {
		let mut cfg = Config::default();
		cfg.dedup_threshold_by_kind[EntityKind::Fact as usize] = Some(1.5);
		assert_eq!(
			cfg.validate().unwrap_err(),
			"dedup_threshold_by_kind[fact] must be in [0.0, 1.0], got 1.5"
		);
		// Bounds are inclusive — 0.0 and 1.0 are accepted.
		cfg.dedup_threshold_by_kind[EntityKind::Fact as usize] = Some(0.0);
		assert!(cfg.validate().is_ok(), "0.0 is in range");
		cfg.dedup_threshold_by_kind[EntityKind::Fact as usize] = Some(1.0);
		assert!(cfg.validate().is_ok(), "1.0 is in range");
		// A NaN is rejected (NaN is not in [0.0, 1.0]).
		cfg.dedup_threshold_by_kind[EntityKind::Claim as usize] = Some(f64::NAN);
		assert!(
			cfg.validate().unwrap_err().contains("claim"),
			"a NaN per-kind threshold is rejected, not silently treated as None"
		);
	}
}
