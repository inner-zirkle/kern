//! Tests extracted from retrieval_intent.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn temporal_queries_lean_lexical() {
		let i = classify_intent("what happened last week in the deploy");
		assert_eq!(i.category, IntentCategory::Temporal);
		assert!(i.lexical_bias > 1.0 && i.dense_bias < 1.0);
		assert!(i.confidence >= 0.3);
	}

	#[test]
	fn procedural_queries_lean_dense() {
		let i = classify_intent("how do I configure the rpc transport");
		assert_eq!(i.category, IntentCategory::Procedural);
		assert!(i.dense_bias > 1.0 && i.importance_bias < 1.0);
	}

	#[test]
	fn preference_queries_lean_importance() {
		let i = classify_intent("which allocator do we prefer for the store");
		assert_eq!(i.category, IntentCategory::Preference);
		assert!(i.importance_bias > 1.0);
	}

	#[test]
	fn unmatched_queries_are_general_and_bias_nothing() {
		let i = classify_intent("zirkle kern graviton");
		assert_eq!(i.category, IntentCategory::General);
		assert_eq!(
			(i.dense_bias, i.lexical_bias, i.importance_bias),
			(1.0, 1.0, 1.0),
			"a General classification must fuse bit-identically to intent off"
		);
	}

	#[test]
	fn more_matches_win_over_fewer() {
		// "when did we decide" hits temporal ("when") once; a date + weekday +
		// relative phrase stacks temporal past any single factual hit.
		let i = classify_intent("what is the status since last monday 2026-01-05");
		assert_eq!(i.category, IntentCategory::Temporal);
		assert!(i.confidence > 0.3);
	}
}
