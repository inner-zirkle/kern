use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::base::constants;
use crate::base::types::Source;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeWeights {
	pub content: f64,
	pub reason: f64,
	pub edge: f64,
}

impl Default for ModeWeights {
	fn default() -> Self {
		Self {
			content: constants::DEFAULT_WEIGHT_CONTENT,
			reason: constants::DEFAULT_WEIGHT_REASON,
			edge: constants::DEFAULT_WEIGHT_EDGE,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
	pub seed_k: usize,
	pub max_expansions: usize,
	pub decay: f64,
	pub qbst_access_weight: f64,
	pub qbst_recency_weight: f64,
	pub qbst_recency_half_life_secs: u64,
	pub qbst_cap: f64,
	pub refine_traversal_weight: f64,
	pub refine_boost_cap: f64,
	// Ceiling on the summed source-weighted edge credit an entity can earn from
	// the walk. 0.0 turns traversal credit off; the cap is what keeps a
	// well-connected node from outranking a direct match on edge volume.
	pub traversal_credit_cap: f64,
	// Multiplier on each edge's credit contribution before the cap.
	pub traversal_credit_weight: f64,
	pub fact_score_boost: f64,
	pub gravity_weight: f64,
	// Multiplier on the final score of an entity held in a `remote-*` phantom kern.
	// Federation is unauthenticated: this is what stops peer-supplied content from
	// outranking local knowledge. 1.0 disables the penalty; 0.0 keeps remote
	// entities retrievable but always last.
	pub remote_trust_weight: f64,
	// Per-source-scheme trust prior, keyed on `Source::scheme()` — file, ticket,
	// session, agent, inline. An absent key is 1.0, so the empty default leaves
	// every score bit-identical. This weights the CHANNEL a claim arrived on, not
	// its author: `kern ingest` and an MCP agent's default ingest both write
	// `inline`, so no key here separates a human from an agent (ROADMAP 20).
	pub source_trust: BTreeMap<String, f64>,
	pub min_deliver_score: f64,
	pub max_deliver_results: usize,
	pub important_min_cosine: f64,
	pub important_access_threshold: i32,
	pub weights_content: ModeWeights,
	pub weights_reason: ModeWeights,
	pub weights_hybrid: ModeWeights,
	pub rrf_k: f64,
	pub rrf_global_weight: f64,
	pub dedup_by_section: bool,
	pub mmr_enabled: bool,
	pub mmr_lambda: f64,
	pub mmr_pool_size: usize,
	pub lexical_enabled: bool,
	pub bm25_k1: f64,
	pub bm25_b: f64,
	// Late-fusion BM25 bonus added to a delivered result's score after the
	// content/edge boosts and before MMR, so an entity whose text shares exact
	// query terms floats to the top of the list. 0.0 disables it and leaves
	// every score bit-identical to the pre-knob baseline. Scaled by the query's
	// max BM25 score so the bonus is magnitude-comparable across corpora.
	pub lexical_top_boost: f64,
	pub pagerank_enabled: bool,
	pub pagerank_damping: f64,
	pub pagerank_iters: usize,
	pub pagerank_top_k: usize,
}

impl Default for RetrievalConfig {
	fn default() -> Self {
		Self {
			seed_k: 15,
			max_expansions: 500,
			decay: 0.25,
			qbst_access_weight: constants::QBST_ACCESS_WEIGHT,
			qbst_recency_weight: constants::QBST_RECENCY_WEIGHT,
			qbst_recency_half_life_secs: constants::QBST_RECENCY_HALF_LIFE.as_secs(),
			qbst_cap: constants::QBST_CAP,
			refine_traversal_weight: constants::REFINE_TRAVERSAL_WEIGHT,
			refine_boost_cap: constants::REFINE_BOOST_CAP,
			traversal_credit_cap: constants::TRAVERSAL_CREDIT_CAP,
			traversal_credit_weight: constants::TRAVERSAL_CREDIT_WEIGHT,
			fact_score_boost: constants::FACT_SCORE_BOOST,
			gravity_weight: 0.15,
			remote_trust_weight: 0.4,
			source_trust: BTreeMap::new(),
			min_deliver_score: 0.0,
			max_deliver_results: 25,
			important_min_cosine: constants::IMPORTANT_MIN_COSINE,
			important_access_threshold: constants::IMPORTANT_ACCESS_THRESHOLD,
			weights_content: ModeWeights {
				content: 0.70,
				reason: 0.15,
				edge: 0.15,
			},
			weights_reason: ModeWeights {
				content: 0.20,
				reason: 0.60,
				edge: 0.20,
			},
			weights_hybrid: ModeWeights::default(),
			rrf_k: 60.0,
			rrf_global_weight: 0.5,
			dedup_by_section: true,
			mmr_enabled: true,
			mmr_lambda: 0.75,
			mmr_pool_size: 50,
			lexical_enabled: true,
			bm25_k1: 1.2,
			bm25_b: 0.75,
			lexical_top_boost: 0.0,
			pagerank_enabled: true,
			pagerank_damping: 0.85,
			pagerank_iters: 25,
			pagerank_top_k: 100,
		}
	}
}

impl RetrievalConfig {
	pub fn validate(&self) -> Vec<String> {
		let mut errs = Vec::new();

		for (name, w) in [
			("content", &self.weights_content),
			("reason", &self.weights_reason),
			("hybrid", &self.weights_hybrid),
		] {
			let sum = w.content + w.reason + w.edge;
			if (sum - 1.0).abs() > 0.01 {
				errs.push(format!("weights_{name} sum to {sum:.3}, expected ~1.0"));
			}
		}

		for (name, v) in [
			("mmr_lambda", self.mmr_lambda),
			("bm25_b", self.bm25_b),
			("remote_trust_weight", self.remote_trust_weight),
		] {
			if !(0.0..=1.0).contains(&v) {
				errs.push(format!("{name} ({v}) must be in [0.0, 1.0]"));
			}
		}

		// A misspelled scheme would weight nothing at all and read as a working
		// knob, so an unknown key is an error rather than a silent no-op.
		for (scheme, w) in &self.source_trust {
			if Source::parse_scheme(scheme).is_none() {
				errs.push(format!(
					"source_trust key {scheme:?} is not a source scheme (file, ticket, session, agent, inline)"
				));
			}
			if !w.is_finite() || *w < 0.0 {
				errs.push(format!("source_trust[{scheme:?}] ({w}) must be >= 0.0"));
			}
		}

		if self.bm25_k1 < 0.0 {
			errs.push(format!("bm25_k1 ({}) must be >= 0.0", self.bm25_k1));
		}
		if !self.lexical_top_boost.is_finite() || self.lexical_top_boost < 0.0 {
			errs.push(format!(
				"lexical_top_boost ({}) must be finite and >= 0.0",
				self.lexical_top_boost
			));
		}

		if !(0.0..1.0).contains(&self.pagerank_damping) {
			errs.push(format!(
				"pagerank_damping ({}) must be in [0.0, 1.0)",
				self.pagerank_damping
			));
		}

		if self.traversal_credit_cap < 0.0 {
			errs.push(format!(
				"traversal_credit_cap ({}) must be >= 0.0",
				self.traversal_credit_cap
			));
		}
		if self.traversal_credit_weight < 0.0 {
			errs.push(format!(
				"traversal_credit_weight ({}) must be >= 0.0",
				self.traversal_credit_weight
			));
		}
		if self.gravity_weight < 0.0 {
			errs.push(format!(
				"gravity_weight ({}) must be >= 0.0",
				self.gravity_weight
			));
		}
		if self.rrf_k < 0.0 {
			errs.push(format!("rrf_k ({}) must be >= 0.0", self.rrf_k));
		}
		if self.seed_k == 0 {
			errs.push("seed_k must be >= 1 (0 seeds nothing, so every query is empty)".to_string());
		}
		if self.max_deliver_results == 0 {
			errs.push("max_deliver_results must be >= 1 (0 delivers nothing)".to_string());
		}

		errs
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_config_is_valid() {
		assert!(
			RetrievalConfig::default().validate().is_empty(),
			"shipped defaults must validate"
		);
	}

	#[test]
	fn weights_not_summing_to_one_are_flagged() {
		let mut cfg = RetrievalConfig::default();
		cfg.weights_hybrid.content = 0.9;
		let errs = cfg.validate();
		assert!(
			errs.iter().any(|e| e.contains("weights_hybrid")),
			"got {errs:?}"
		);
	}

	#[test]
	fn out_of_range_bm25_params_are_flagged() {
		let bad_b = RetrievalConfig {
			bm25_b: 2.0,
			..Default::default()
		};
		assert!(
			bad_b.validate().iter().any(|e| e.contains("bm25_b")),
			"bm25_b > 1"
		);

		let neg_k1 = RetrievalConfig {
			bm25_k1: -0.5,
			..Default::default()
		};
		assert!(
			neg_k1.validate().iter().any(|e| e.contains("bm25_k1")),
			"negative bm25_k1"
		);
	}

	// A typo'd scheme weights nothing and reads exactly like a working knob, which
	// is the failure an operator cannot see from the ranking.
	#[test]
	fn an_unknown_or_negative_source_trust_key_is_flagged() {
		let typo = RetrievalConfig {
			source_trust: BTreeMap::from([("files".to_string(), 0.5)]),
			..Default::default()
		};
		assert!(
			typo.validate().iter().any(|e| e.contains("files")),
			"got {:?}",
			typo.validate()
		);

		let negative = RetrievalConfig {
			source_trust: BTreeMap::from([("file".to_string(), -0.5)]),
			..Default::default()
		};
		assert!(
			negative.validate().iter().any(|e| e.contains("file")),
			"got {:?}",
			negative.validate()
		);

		let ok = RetrievalConfig {
			source_trust: BTreeMap::from([("agent".to_string(), 1.5)]),
			..Default::default()
		};
		assert!(
			ok.validate().is_empty(),
			"a real scheme above 1.0 lifts it over baseline and is valid: {:?}",
			ok.validate()
		);
	}

	#[test]
	fn retrieval_breaking_values_are_flagged() {
		let neg_rrf = RetrievalConfig {
			rrf_k: -1.0,
			..Default::default()
		};
		assert!(
			neg_rrf.validate().iter().any(|e| e.contains("rrf_k")),
			"negative rrf_k"
		);

		let zero_seed = RetrievalConfig {
			seed_k: 0,
			..Default::default()
		};
		assert!(
			zero_seed.validate().iter().any(|e| e.contains("seed_k")),
			"seed_k 0"
		);

		let zero_deliver = RetrievalConfig {
			max_deliver_results: 0,
			..Default::default()
		};
		assert!(
			zero_deliver
				.validate()
				.iter()
				.any(|e| e.contains("max_deliver_results")),
			"max_deliver_results 0"
		);

		let neg_gravity = RetrievalConfig {
			gravity_weight: -0.1,
			..Default::default()
		};
		assert!(
			neg_gravity
				.validate()
				.iter()
				.any(|e| e.contains("gravity_weight")),
			"negative gravity_weight"
		);

		let zero_rrf = RetrievalConfig {
			rrf_k: 0.0,
			..Default::default()
		};
		assert!(
			!zero_rrf.validate().iter().any(|e| e.contains("rrf_k")),
			"rrf_k 0 is valid, must not flag"
		);

		let neg_boost = RetrievalConfig {
			lexical_top_boost: -0.1,
			..Default::default()
		};
		assert!(
			neg_boost
				.validate()
				.iter()
				.any(|e| e.contains("lexical_top_boost")),
			"negative lexical_top_boost"
		);
	}
}
