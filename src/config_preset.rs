use serde::{Deserialize, Serialize};

use crate::config::Config;

// The whole tuning surface: heat, dedup, and retrieval breadth belong to the
// preset, not to individual keys. `Config::load` refuses the [heat]/[ingest]/
// [retrieval] sections — `[ingest] review_policy` is the one key it lets
// through, and it is curation, not tuning — and `apply` runs AFTER the file is
// deserialized, so it is the only effective writer of these knobs regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
	#[default]
	Relaxed,
	Medium,
	Tight,
}

struct Tuning {
	half_life_secs: u64,
	dedup_threshold: f64,
	seed_k: usize,
	max_expansions: usize,
	max_deliver_results: usize,
}

impl Preset {
	/// Lowercase name matching the serde rendering, for health surfacing.
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Relaxed => "relaxed",
			Self::Medium => "medium",
			Self::Tight => "tight",
		}
	}

	fn tuning(&self) -> Tuning {
		match self {
			Self::Relaxed => Tuning {
				half_life_secs: 30 * 24 * 60 * 60,
				dedup_threshold: 0.98,
				seed_k: 25,
				max_expansions: 800,
				max_deliver_results: 40,
			},
			Self::Medium => Tuning {
				half_life_secs: 7 * 24 * 60 * 60,
				dedup_threshold: 0.95,
				seed_k: 15,
				max_expansions: 500,
				max_deliver_results: 25,
			},
			Self::Tight => Tuning {
				half_life_secs: 3 * 24 * 60 * 60,
				dedup_threshold: 0.90,
				seed_k: 10,
				max_expansions: 250,
				max_deliver_results: 12,
			},
		}
	}

	pub(crate) fn apply(&self, cfg: &mut Config) {
		let t = self.tuning();
		cfg.heat.half_life_secs = t.half_life_secs;
		cfg.ingest.dedup_threshold = t.dedup_threshold;
		cfg.retrieval.seed_k = t.seed_k;
		cfg.retrieval.max_expansions = t.max_expansions;
		cfg.retrieval.max_deliver_results = t.max_deliver_results;
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::config::{IngestConfig, RetrievalConfig};
	use crate::heat::HeatConfig;
	use std::path::Path;

	fn applied(p: Preset) -> Config {
		let mut cfg = Config::default_in(Path::new("x"));
		p.apply(&mut cfg);
		cfg
	}

	#[test]
	fn every_preset_yields_a_valid_config() {
		for p in [Preset::Relaxed, Preset::Medium, Preset::Tight] {
			assert!(applied(p).validate().is_ok(), "{p:?} must validate");
		}
	}

	#[test]
	fn medium_matches_the_neutral_struct_defaults() {
		// The sub-config defaults are the medium anchor; this pins them together
		// so neither can drift without failing here.
		let t = Preset::Medium.tuning();
		let r = RetrievalConfig::default();
		assert_eq!(t.half_life_secs, HeatConfig::default().half_life_secs);
		assert_eq!(t.dedup_threshold, IngestConfig::default().dedup_threshold);
		assert_eq!(t.seed_k, r.seed_k);
		assert_eq!(t.max_expansions, r.max_expansions);
		assert_eq!(t.max_deliver_results, r.max_deliver_results);
	}

	#[test]
	fn relaxed_and_tight_move_the_knobs_in_opposite_directions() {
		let r = applied(Preset::Relaxed);
		let m = applied(Preset::Medium);
		let t = applied(Preset::Tight);
		assert!(r.heat.half_life_secs > m.heat.half_life_secs);
		assert!(t.heat.half_life_secs < m.heat.half_life_secs);
		assert!(r.retrieval.max_deliver_results > m.retrieval.max_deliver_results);
		assert!(t.retrieval.max_deliver_results < m.retrieval.max_deliver_results);
		assert!(r.ingest.dedup_threshold > m.ingest.dedup_threshold);
		assert!(t.ingest.dedup_threshold < m.ingest.dedup_threshold);
	}

	#[test]
	fn the_default_preset_is_relaxed() {
		assert_eq!(Preset::default(), Preset::Relaxed);
	}
}
