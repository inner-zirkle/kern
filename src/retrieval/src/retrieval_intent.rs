//! Query intent classification: regex-only, no LLM, run once per query to bias
//! the hybrid RRF fusion. A temporal question leans on exact terms (BM25), a
//! procedural one on semantics (dense), an entity/preference one on standing
//! importance. Ported from mnemosyne's `query_intent` (MIT), remapped from its
//! vector/FTS/importance triple onto kern's dense/lexical/importance RRF lists.

use regex::Regex;
use std::sync::LazyLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentCategory {
	Temporal,
	Factual,
	Entity,
	Preference,
	Procedural,
	General,
}

impl IntentCategory {
	pub fn as_str(self) -> &'static str {
		match self {
			IntentCategory::Temporal => "temporal",
			IntentCategory::Factual => "factual",
			IntentCategory::Entity => "entity",
			IntentCategory::Preference => "preference",
			IntentCategory::Procedural => "procedural",
			IntentCategory::General => "general",
		}
	}
}

/// The classification plus the three fusion biases it implies. `General` is
/// all-1.0, so an unclassifiable query fuses bit-identically to the
/// pre-intent pipeline.
#[derive(Debug, Clone, Copy)]
pub struct QueryIntent {
	pub category: IntentCategory,
	pub confidence: f64,
	pub dense_bias: f64,
	pub lexical_bias: f64,
	pub importance_bias: f64,
}

impl QueryIntent {
	pub fn general() -> Self {
		Self {
			category: IntentCategory::General,
			confidence: 0.0,
			dense_bias: 1.0,
			lexical_bias: 1.0,
			importance_bias: 1.0,
		}
	}
}

// (dense, lexical, importance) per category. The temporal row is the load-
// bearing one: "when/last week" questions die on pure cosine similarity and
// live on exact term overlap.
fn biases(category: IntentCategory) -> (f64, f64, f64) {
	match category {
		IntentCategory::Temporal => (0.6, 1.5, 0.8),
		IntentCategory::Factual => (1.0, 1.2, 0.9),
		IntentCategory::Entity => (1.1, 1.0, 1.3),
		IntentCategory::Preference => (0.9, 0.8, 1.5),
		IntentCategory::Procedural => (1.3, 0.9, 0.7),
		IntentCategory::General => (1.0, 1.0, 1.0),
	}
}

static INTENT_PATTERNS: LazyLock<Vec<(IntentCategory, Vec<Regex>)>> = LazyLock::new(|| {
	let compile = |pats: &[&str]| -> Vec<Regex> {
		pats
			.iter()
			.map(|p| Regex::new(p).expect("built-in intent pattern"))
			.collect()
	};
	vec![
		(
			IntentCategory::Temporal,
			compile(&[
				r"\b(when|last|yesterday|today|tomorrow|ago|before|after|since|until|during|recently|lately)\b",
				r"\b(monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b",
				r"\b(january|february|march|april|may|june|july|august|september|october|november|december)\b",
				r"\b\d{4}-\d{2}-\d{2}\b",
				r"\b\d{1,2}[/-]\d{1,2}[/-]\d{2,4}\b",
				r"\b(this|next|last)\s+(week|month|year)\b",
				r"\b\d+\s+(day|week|month|year|hour|minute)s?\s+(ago|from now|later|earlier)\b",
			]),
		),
		(
			IntentCategory::Factual,
			compile(&[
				r"\bwhat\s+is\b",
				r"\bwho\s+is\b",
				r"\bwhere\s+is\b",
				r"\b(definition|define|explain|meaning)\b",
				r"\bhow\s+(many|much|long|far)\b",
			]),
		),
		(
			IntentCategory::Entity,
			compile(&[
				r"\b(tell\s+me\s+about|what\s+do\s+you\s+know\s+about)\b",
				r"\b(who\s+is|what\s+does)\s+[a-z]+\b",
				r"\b(about|regarding|concerning)\s+[a-z]+\b",
			]),
		),
		(
			IntentCategory::Preference,
			compile(&[
				r"\b(prefer|like|dislike|want|hate|love|enjoy|favorite|best|worst)\b",
				r"\b(should\s+i|would\s+you|do\s+you\s+recommend)\b",
				r"\b(choose|pick|select|option|choice|decide)\b",
			]),
		),
		(
			IntentCategory::Procedural,
			compile(&[
				r"\bhow\s+(to|do|can|should|would)\b",
				r"\b(step|process|procedure|workflow|guide|tutorial)\b",
				r"\b(setup|install|configure|build|deploy|run|execute|start|stop)\b",
			]),
		),
	]
});

/// Classify one query. Every category is scored (base 0.3 + 0.15 per matched
/// pattern, capped at 1.0) and the best wins; no match at all is `General`,
/// which biases nothing.
///
/// The bias is scaled by confidence — `1 + confidence × (bias − 1)` — so the
/// weights move in proportion to the evidence. One incidental keyword in a
/// statement-shaped probe ("the deploy pipeline runs on Jenkins…") nudges the
/// fusion a few percent; a question that stacks signals ("how do I configure
/// the transport") gets most of the table. Full-strength bias at low
/// confidence demonstrably re-ranked linked neighbours out of the top 5 (the
/// reason-edge reachability invariant), which no fusion tweak is worth.
pub fn classify_intent(query_text: &str) -> QueryIntent {
	let lower = query_text.to_lowercase();
	let mut best = IntentCategory::General;
	let mut best_score = 0.0_f64;
	for (category, patterns) in INTENT_PATTERNS.iter() {
		let matches = patterns.iter().filter(|re| re.is_match(&lower)).count();
		if matches == 0 {
			continue;
		}
		let score = (0.3 + matches as f64 * 0.15).min(1.0);
		if score > best_score {
			best_score = score;
			best = *category;
		}
	}
	let (dense_bias, lexical_bias, importance_bias) = biases(best);
	let scale = |b: f64| 1.0 + best_score * (b - 1.0);
	QueryIntent {
		category: best,
		confidence: best_score,
		dense_bias: scale(dense_bias),
		lexical_bias: scale(lexical_bias),
		importance_bias: scale(importance_bias),
	}
}

#[cfg(test)]
#[path = "tests/retrieval_intent_test.rs"]
mod retrieval_intent_tests;
