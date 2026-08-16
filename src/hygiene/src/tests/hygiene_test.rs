//! Tests extracted from hygiene.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn empty_content_scores_one() {
		let s = score_noise("   \n  ", 0.6);
		assert_eq!(s.score, 1.0);
		assert_eq!(s.reasons, vec!["empty_content".to_string()]);
	}

	#[test]
	fn secrets_are_labelled_and_never_echoed() {
		let s = score_noise("deploy key AKIAIOSFODNN7EXAMPLE for staging", 0.6);
		assert!(s.score >= 0.9);
		assert_eq!(s.secrets, vec!["aws_access_key"]);
		assert!(
			s.reasons
				.iter()
				.any(|r| r == "secret_detected:aws_access_key"),
			"reason names the label, not the value: {:?}",
			s.reasons
		);
	}

	#[test]
	fn value_keyword_clamps_score_down_but_not_over_secrets() {
		// A noisy-looking line rescued by a value keyword.
		let rescued = score_noise("ok. we prefer tabs over spaces in this repo", 0.6);
		assert!(
			rescued.score <= 0.3,
			"value keyword clamps to at most 0.3, got {}",
			rescued.score
		);
		// The clamp is skipped when a secret is present — a secret outranks usefulness.
		let secret = score_noise(
			"we prefer this token: ghp_0123456789abcdefghijklmnopqrstuvwxyz",
			0.6,
		);
		assert!(
			secret.score >= 0.9,
			"secret keeps the score high: {}",
			secret.score
		);
	}

	#[test]
	fn scoring_is_max_not_additive() {
		// Several weak signals must not compound past the strongest one:
		// trivial_keyword (0.7) + low_importance (0.5) stays 0.7, not 1.2.
		let s = score_noise("acknowledged", 0.1);
		assert!(
			(s.score - 0.7).abs() < 1e-9,
			"trivial keyword caps at 0.7, got {}",
			s.score
		);
		assert!(s.reasons.iter().any(|r| r == "low_importance"));
	}

	#[test]
	fn terminal_output_and_stack_traces_score_high() {
		let term = score_noise("Successfully installed requests-2.31.0", 0.6);
		assert!(term.score >= 0.8);
		let trace = score_noise(
			"Traceback (most recent call last):\n  File \"x.py\", line 1",
			0.6,
		);
		assert!(trace.score >= 0.85);
		let rust_panic = score_noise("thread 'main' panicked at src/main.rs:10:5", 0.6);
		assert!(rust_panic.score >= 0.8, "got {}", rust_panic.score);
	}

	#[test]
	fn low_confidence_is_a_weak_signal() {
		let s = score_noise("some perfectly ordinary sentence about the weather", 0.1);
		assert!(s.score >= 0.5 && s.score < 0.8);
		assert!(s.reasons.iter().any(|r| r == "low_importance"));
	}

	#[test]
	fn suggested_action_ladder() {
		assert_eq!(suggest_action(0.9, true), SuggestedAction::Flag);
		assert_eq!(suggest_action(0.85, false), SuggestedAction::Delete);
		assert_eq!(suggest_action(0.6, false), SuggestedAction::Archive);
		assert_eq!(suggest_action(0.2, false), SuggestedAction::Keep);
	}

	#[test]
	fn gate_off_allows_everything() {
		let gate = GateConfig::default();
		assert!(matches!(
			gate_write("AKIAIOSFODNN7EXAMPLE", &gate),
			GateDecision::Allow
		));
	}

	#[test]
	fn gate_strict_rejects_secrets_and_noise() {
		let gate = GateConfig {
			mode: GateMode::Strict,
			extra_patterns: Vec::new(),
		};
		let GateDecision::Reject(rej) = gate_write("token AKIAIOSFODNN7EXAMPLE", &gate) else {
			panic!("secret must reject under strict");
		};
		assert_eq!(rej.secrets, vec!["aws_access_key"]);
		assert!(matches!(
			gate_write("npm warn deprecated foo@1.0.0", &gate),
			GateDecision::Reject(_)
		));
		assert!(matches!(
			gate_write("the auth service owns session invalidation", &gate),
			GateDecision::Allow
		));
	}

	#[test]
	fn gate_warn_carries_the_classification_but_allows() {
		let gate = GateConfig {
			mode: GateMode::Warn,
			extra_patterns: Vec::new(),
		};
		assert!(matches!(
			gate_write("npm warn deprecated foo@1.0.0", &gate),
			GateDecision::Warn(_)
		));
	}

	#[test]
	fn operator_patterns_reject_and_invalid_ones_refuse_compile() {
		let extra = compile_patterns(&["(?i)scratch note".to_string()]).unwrap();
		let gate = GateConfig {
			mode: GateMode::Strict,
			extra_patterns: extra,
		};
		let GateDecision::Reject(rej) = gate_write("Scratch note: try later", &gate) else {
			panic!("operator pattern must reject under strict");
		};
		assert_eq!(rej.reason, "ignore_pattern_match");
		assert!(compile_patterns(&["([unclosed".to_string()]).is_err());
	}

	#[test]
	fn gate_mode_parses_exactly_three_values() {
		assert_eq!(GateMode::parse("off"), Some(GateMode::Off));
		assert_eq!(GateMode::parse("warn"), Some(GateMode::Warn));
		assert_eq!(GateMode::parse("strict"), Some(GateMode::Strict));
		assert_eq!(GateMode::parse("Strict"), None);
	}
}
