//! Tests extracted from heat.rs
#![allow(unused)]
use super::*;

use config::HeatConfig;
mod tests {
	use super::*;
	use std::time::Duration;

	const HL: u64 = 100;

	#[test]
	fn decayed_zero_or_negative_heat_is_zero() {
		let now = SystemTime::now();
		assert_eq!(decayed(0.0, Some(now), now, HL), 0.0);
		assert_eq!(
			decayed(-5.0, Some(now), now, HL),
			0.0,
			"guard clamps non-positive heat"
		);
	}

	#[test]
	fn decayed_none_since_returns_heat_unchanged() {
		assert_eq!(decayed(3.0, None, SystemTime::now(), HL), 3.0);
	}

	#[test]
	fn decayed_clock_skew_returns_heat_unchanged() {
		let now = SystemTime::now();
		let since = now + Duration::from_secs(60);
		assert_eq!(decayed(4.0, Some(since), now, HL), 4.0);
	}

	#[test]
	fn decayed_one_half_life_halves_the_heat() {
		let since = SystemTime::UNIX_EPOCH;
		let now = since + Duration::from_secs(HL);
		let got = decayed(8.0, Some(since), now, HL);
		assert!(
			(got - 4.0).abs() < 1e-4,
			"one half-life halves 8 -> ~4, got {got}"
		);
		let now2 = since + Duration::from_secs(2 * HL);
		let got2 = decayed(8.0, Some(since), now2, HL);
		assert!(
			(got2 - 2.0).abs() < 1e-4,
			"two half-lives -> ~2, got {got2}"
		);
	}

	#[test]
	fn decayed_zero_half_life_is_clamped_to_one_second() {
		let since = SystemTime::UNIX_EPOCH;
		let now = since + Duration::from_secs(10);
		let got = decayed(8.0, Some(since), now, 0);
		assert!(
			got.is_finite() && got >= 0.0,
			"no NaN/inf for zero half-life, got {got}"
		);
		assert!(
			got < 0.01,
			"10s over a clamped 1s half-life decays heavily, got {got}"
		);
	}

	#[test]
	fn deposit_adds_on_top_of_the_decayed_value() {
		let since = SystemTime::UNIX_EPOCH;
		let now = since + Duration::from_secs(HL);
		let got = deposit(8.0, Some(since), now, HL, 1.5);
		assert!(
			(got - 5.5).abs() < 1e-4,
			"decayed (~4) + deposit (1.5) = ~5.5, got {got}"
		);
	}

	#[test]
	fn config_default_is_a_one_week_half_life() {
		let c = HeatConfig::default();
		assert_eq!(c.half_life_secs, 7 * 24 * 60 * 60);
		assert_eq!(c.deposit_access, 1.0);
	}

	#[test]
	fn weibull_default_curve_is_bit_identical_to_exponential() {
		let since = SystemTime::UNIX_EPOCH;
		for secs in [0u64, 1, HL / 2, HL, HL * 3, HL * 10] {
			let now = since + Duration::from_secs(secs);
			assert_eq!(
				decayed_weibull(8.0, Some(since), now, HL, KindDecay::default()),
				decayed(8.0, Some(since), now, HL),
				"default curve must equal the exponential at t={secs}"
			);
		}
	}

	#[test]
	fn preference_curve_outlives_the_default_and_unknown_labels_change_nothing() {
		let since = SystemTime::UNIX_EPOCH;
		let now = since + Duration::from_secs(HL * 4);
		let pref = decayed_weibull(8.0, Some(since), now, HL, kind_decay("preference"));
		let plain = decayed_weibull(8.0, Some(since), now, HL, kind_decay(""));
		assert!(
			pref > plain,
			"a preference (k<1, wide η) must hold heat longer: {pref} vs {plain}"
		);
		assert_eq!(kind_decay("no-such-kind").shape, 1.0);
		assert_eq!(kind_decay("no-such-kind").eta_factor, 1.0);
	}

	#[test]
	fn claim_kind_label_reads_only_the_session_title_convention() {
		let distilled = base::base_types::Source::Session {
			session_id: "s".into(),
			section: String::new(),
			title: "session://preference".into(),
		};
		assert_eq!(claim_kind_label(&distilled), "preference");
		let file = base::base_types::Source::File {
			path: "/a".into(),
			section: String::new(),
			title: "a".into(),
			author: String::new(),
			url: String::new(),
		};
		assert_eq!(claim_kind_label(&file), "", "non-claims carry no label");
	}

	#[test]
	fn increasing_hazard_decays_faster_late() {
		// shape > 1: survival at 4 half-lives is BELOW the exponential's.
		let since = SystemTime::UNIX_EPOCH;
		let now = since + Duration::from_secs(HL * 4);
		let fast = decayed_weibull(
			8.0,
			Some(since),
			now,
			HL,
			KindDecay {
				shape: 1.5,
				eta_factor: 1.0,
			},
		);
		let plain = decayed(8.0, Some(since), now, HL);
		assert!(
			fast < plain,
			"k>1 ages out faster at large t: {fast} vs {plain}"
		);
	}
}
