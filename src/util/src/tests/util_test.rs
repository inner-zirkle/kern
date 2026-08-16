//! Tests extracted from util.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn hex_encode_is_lowercase_two_chars_per_byte() {
		assert_eq!(hex::encode([0x00, 0xff, 0x10, 0xab]), "00ff10ab");
		assert_eq!(hex::encode([]), "");
	}

	#[test]
	fn hex_decode_roundtrips_and_rejects_bad_input() {
		assert_eq!(hex::decode(""), Some(vec![]));
		assert_eq!(hex::decode("00ff10ab"), Some(vec![0x00, 0xff, 0x10, 0xab]));
		assert_eq!(hex::decode("00FF10AB"), Some(vec![0x00, 0xff, 0x10, 0xab]));
		assert_eq!(hex::decode("ed25519:00ff"), Some(vec![0x00, 0xff]));
		assert_eq!(hex::decode("0"), None, "odd length");
		assert_eq!(hex::decode("00ff10ag"), None, "non-hex digit");
		assert_eq!(hex::encode(hex::decode("deadbeef").unwrap()), "deadbeef");
	}

	#[test]
	fn percentile_sorted_is_nearest_rank_with_edges_and_generic_types() {
		let xs: Vec<f64> = (1..=10).map(|i| i as f64).collect();
		assert_eq!(percentile_sorted(&xs, 0.0), Some(1.0), "p<=0 -> first");
		assert_eq!(percentile_sorted(&xs, 1.0), Some(10.0), "p>=1 -> last");
		assert_eq!(
			percentile_sorted(&xs, 0.5),
			Some(5.0),
			"ceil(0.5*10)=5 -> xs[4]"
		);
		assert_eq!(percentile_sorted(&xs, 0.95), Some(10.0));
		assert_eq!(percentile_sorted::<f64>(&[], 0.5), None, "empty -> None");
		let ns: Vec<u128> = vec![10, 20, 30, 40, 50];
		assert_eq!(percentile_sorted(&ns, 0.5), Some(30u128));
		assert_eq!(percentile_sorted(&ns, 0.95), Some(50u128));
	}

	#[test]
	fn cmp_rank_orders_by_score_desc_then_id_asc() {
		use std::cmp::Ordering;
		assert_eq!(cmp_rank(0.9_f64, "z", 0.1, "a"), Ordering::Less);
		assert_eq!(cmp_rank(0.1_f64, "a", 0.9, "z"), Ordering::Greater);
		assert_eq!(cmp_rank(0.5_f64, "a", 0.5, "b"), Ordering::Less);
		assert_eq!(cmp_rank(0.5_f64, "b", 0.5, "a"), Ordering::Greater);
		assert_eq!(cmp_rank(0.5_f64, "a", 0.5, "a"), Ordering::Equal);
		assert_eq!(cmp_rank(f64::NAN, "a", f64::NAN, "b"), Ordering::Less);
		assert_eq!(cmp_rank(2.0_f32, "a", 1.0_f32, "z"), Ordering::Less);
	}

	#[test]
	fn content_hash_is_deterministic_64_char_lowercase_hex() {
		let h = content_hash("kern");
		assert_eq!(h.len(), 64, "sha256 -> 32 bytes -> 64 hex chars");
		assert!(h
			.bytes()
			.all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
		assert_eq!(h, content_hash("kern"), "deterministic");
		assert_ne!(h, content_hash("kern2"), "distinct inputs differ");
	}

	#[test]
	fn short_id_caps_at_12_chars_and_is_boundary_safe() {
		assert_eq!(short_id("0123456789abcdef"), "0123456789ab");
		assert_eq!(short_id("abc"), "abc");
		assert_eq!(short_id("0123456789ab"), "0123456789ab");
		let s = short_id("ααααααααααααββ");
		assert_eq!(s.chars().count(), 12);
	}

	#[test]
	fn truncate_appends_ellipsis_only_when_cut() {
		assert_eq!(truncate("hello", 10), "hello", "under max -> unchanged");
		assert_eq!(
			truncate("hello world", 5),
			"hello...",
			"over max -> cut + ellipsis"
		);
		assert_eq!(truncate("αβγδε", 3), "αβγ...");
	}

	#[test]
	fn cmp_partial_orders_and_treats_nan_as_equal() {
		use std::cmp::Ordering;
		assert_eq!(cmp_partial(&1.0, &2.0), Ordering::Less);
		assert_eq!(cmp_partial(&2.0, &1.0), Ordering::Greater);
		assert_eq!(cmp_partial(&1.0, &1.0), Ordering::Equal);
		assert_eq!(
			cmp_partial(&f64::NAN, &1.0),
			Ordering::Equal,
			"NaN is incomparable -> Equal"
		);
	}

	#[test]
	fn uuid_v4_has_correct_layout_version_and_variant() {
		let u = uuid_v4();
		let groups: Vec<&str> = u.split('-').collect();
		assert_eq!(
			groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
			vec![8, 4, 4, 4, 12],
			"5 dash-separated groups of 8-4-4-4-12"
		);
		assert!(u.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit()));
		assert_eq!(&groups[2][0..1], "4", "RFC4122 version 4");
		assert!(
			matches!(&groups[3][0..1], "8" | "9" | "a" | "b"),
			"RFC4122 variant bits"
		);
		assert_ne!(uuid_v4(), uuid_v4(), "two mints differ (random)");
	}

	#[test]
	fn now_nanos_is_after_epoch() {
		assert!(now_nanos() > 0);
	}
}
mod validate_tests {
	use super::*;

	#[test]
	fn conf_out_of_range_rejected_high() {
		assert!(matches!(
			validate_conf(1.5),
			Err(ValidateError::ConfOutOfRange(_))
		));
	}

	#[test]
	fn conf_out_of_range_rejected_low() {
		assert!(matches!(
			validate_conf(-0.01),
			Err(ValidateError::ConfOutOfRange(_))
		));
	}

	#[test]
	fn conf_out_of_range_rejected_nan() {
		assert!(matches!(
			validate_conf(f64::NAN),
			Err(ValidateError::ConfOutOfRange(_))
		));
	}

	#[test]
	fn conf_inclusive_bounds_accepted() {
		assert_eq!(validate_conf(0.0), Ok(0.0));
		assert_eq!(validate_conf(1.0), Ok(1.0));
		assert_eq!(validate_conf(0.5), Ok(0.5));
	}
}
mod throttle_tests {
	use super::*;

	#[test]
	fn the_first_call_passes_and_the_flood_behind_it_does_not() {
		let t = LogThrottle::new(3600);
		assert!(t.allow(), "the first crossing is always reported");
		for _ in 0..1000 {
			assert!(!t.allow(), "every later call inside the window is silent");
		}
	}

	#[test]
	fn a_zero_interval_never_throttles() {
		let t = LogThrottle::new(0);
		assert!(t.allow());
		assert!(t.allow(), "interval 0 disables throttling");
	}
}
mod time_tests {
	use super::parse_rfc3339;

	#[test]
	fn valid_timestamps_parse() {
		assert!(parse_rfc3339("2026-06-05T09:00:00Z").is_ok());
		assert!(parse_rfc3339("2026-06-05T09:00:00").is_ok());
		assert!(parse_rfc3339("  2026-06-05T09:00:00Z  ").is_ok());
	}

	#[test]
	fn short_after_trim_is_err_not_panic() {
		assert_eq!(parse_rfc3339("   2026   "), Err(()));
		assert_eq!(parse_rfc3339("                    "), Err(()));
		assert_eq!(parse_rfc3339(""), Err(()));
	}

	#[test]
	fn multibyte_in_slice_region_is_err_not_panic() {
		assert_eq!(parse_rfc3339("20é6-06-05T09:00:00Z"), Err(()));
		assert_eq!(parse_rfc3339("2026-06-05T09:00:0😀"), Err(()));
	}

	#[test]
	fn malformed_digits_are_err() {
		assert_eq!(parse_rfc3339("YYYY-06-05T09:00:00Z"), Err(()));
	}

	#[test]
	fn epoch_and_known_instant_compute_correctly() {
		use std::time::{Duration, UNIX_EPOCH};
		assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Ok(UNIX_EPOCH));
		// 2000-01-01T00:00:00Z = 946684800 unix seconds.
		assert_eq!(
			parse_rfc3339("2000-01-01T00:00:00Z"),
			Ok(UNIX_EPOCH + Duration::from_secs(946684800))
		);
	}

	#[test]
	fn civil_from_days_at_epoch_is_1970_01_01() {
		assert_eq!(super::civil_from_days(0), (1970, 1, 1));
	}

	#[test]
	fn civil_from_days_round_trips_a_known_date() {
		// 2026-07-22 is 20656 days after 1970-01-01.
		assert_eq!(super::civil_from_days(20656), (2026, 7, 22));
	}

	#[test]
	fn date_string_renders_epoch_and_a_known_instant() {
		assert_eq!(super::date_string(std::time::UNIX_EPOCH), "1970-01-01");
		let t = super::parse_rfc3339("2026-07-22T00:00:00").unwrap();
		assert_eq!(super::date_string(t), "2026-07-22");
	}
}
