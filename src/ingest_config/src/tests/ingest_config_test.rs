//! Tests extracted from ingest_config.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::EntityKind;

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
