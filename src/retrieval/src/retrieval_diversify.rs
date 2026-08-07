//! Result diversification, the last rank stage: collapse near-duplicate hits
//! from one source section, then MMR-reorder so each kept result trades
//! relevance against similarity to what is already kept.

use crate::retrieval::expand::Scored;
use config::RetrievalConfig;
use math::cosine;
use std::collections::HashMap;

pub fn dedup_by_section<T: Scored>(cfg: &RetrievalConfig, results: &mut Vec<T>) {
	if !cfg.dedup_by_section {
		return;
	}
	let mut best: HashMap<String, usize> = HashMap::new();
	let mut keep: Vec<bool> = vec![true; results.len()];
	for (i, r) in results.iter().enumerate() {
		let section = section_key(r.entity().source.section());
		if section.is_empty() {
			continue;
		}
		match best.get(&section).copied() {
			Some(j) => {
				if results[j].score() >= r.score() {
					keep[i] = false;
				} else {
					keep[j] = false;
					best.insert(section, i);
				}
			}
			None => {
				best.insert(section, i);
			}
		}
	}
	let mut idx = 0;
	results.retain(|_| {
		let k = keep[idx];
		idx += 1;
		k
	});
}

fn section_key(section: &str) -> String {
	match section.find("#chunk") {
		Some(i) => section[..i].to_string(),
		None => section.to_string(),
	}
}

pub fn mmr<T: Scored>(cfg: &RetrievalConfig, query_vec: &[f32], results: &mut Vec<T>) {
	if !cfg.mmr_enabled || results.len() <= cfg.max_deliver_results {
		return;
	}
	let pool_size = cfg.mmr_pool_size.min(results.len());
	if pool_size == 0 {
		return;
	}
	let target = cfg.max_deliver_results.min(pool_size);
	let lambda = cfg.mmr_lambda;

	let tail = results.split_off(pool_size);
	let mut pool: Vec<T> = std::mem::take(results);

	let query_usable = !query_vec.is_empty();
	let mut sim_q: Vec<f64> = pool
		.iter()
		.map(|cand| {
			if query_usable && !cand.entity().vector.is_empty() {
				cosine(query_vec, &cand.entity().vector)
			} else {
				cand.score()
			}
		})
		.collect();

	let mut max_sim: Vec<f64> = vec![0.0; pool.len()];

	let mut selected: Vec<T> = Vec::with_capacity(target);

	while selected.len() < target && !pool.is_empty() {
		let mut best_i = 0usize;
		let mut best_score = f64::NEG_INFINITY;
		for i in 0..pool.len() {
			let mmr_val = lambda * sim_q[i] - (1.0 - lambda) * max_sim[i];
			if mmr_val > best_score {
				best_score = mmr_val;
				best_i = i;
			}
		}
		// sim_q and max_sim swap-removed in lockstep with pool so index i stays aligned.
		let chosen = pool.swap_remove(best_i);
		sim_q.swap_remove(best_i);
		max_sim.swap_remove(best_i);

		if !chosen.entity().vector.is_empty() {
			for (j, cand) in pool.iter().enumerate() {
				if !cand.entity().vector.is_empty() {
					let s = cosine(&chosen.entity().vector, &cand.entity().vector);
					if s > max_sim[j] {
						max_sim[j] = s;
					}
				}
			}
		}

		selected.push(chosen);
	}

	*results = selected;
	results.extend(tail);
	results.truncate(cfg.max_deliver_results);
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::retrieval::expand::ScoredEntity;
	use base::base_types::{Entity, Source};

	fn sect(id: &str, section: &str, score: f64) -> ScoredEntity {
		ScoredEntity {
			entity: Entity {
				id: id.into(),
				source: Source::Inline {
					hash: id.into(),
					section: section.into(),
				},
				..Default::default()
			},
			score,
		}
	}

	#[test]
	fn dedup_keeps_highest_per_section() {
		let cfg = RetrievalConfig::default();
		let mut results = vec![
			sect("a", "doc#chunk0", 0.4),
			sect("b", "doc#chunk1", 0.9),
			sect("c", "other#chunk0", 0.5),
		];
		dedup_by_section(&cfg, &mut results);
		let ids: Vec<&str> = results.iter().map(|r| r.entity.id.as_str()).collect();
		assert!(ids.contains(&"b"), "highest in section kept: {ids:?}");
		assert!(
			!ids.contains(&"a"),
			"lower in same section dropped: {ids:?}"
		);
		assert!(ids.contains(&"c"), "distinct section kept: {ids:?}");
		assert_eq!(results.len(), 2);
	}

	#[test]
	fn dedup_keeps_empty_section_entries() {
		let cfg = RetrievalConfig::default();
		let mut results = vec![sect("a", "", 0.1), sect("b", "", 0.2)];
		dedup_by_section(&cfg, &mut results);
		assert_eq!(
			results.len(),
			2,
			"empty-section entries are never collapsed"
		);
	}

	#[test]
	fn dedup_preserves_relative_order_of_survivors() {
		let cfg = RetrievalConfig::default();
		let mut results = vec![
			sect("first", "alpha#chunk0", 0.5),
			sect("second", "beta#chunk0", 0.9),
			sect("beta_low", "beta#chunk1", 0.2),
			sect("third", "gamma#chunk0", 0.7),
		];
		dedup_by_section(&cfg, &mut results);
		let ids: Vec<&str> = results.iter().map(|r| r.entity.id.as_str()).collect();
		assert_eq!(
			ids,
			vec!["first", "second", "third"],
			"survivors keep original order"
		);
	}

	#[test]
	fn dedup_noop_when_disabled() {
		let cfg = RetrievalConfig {
			dedup_by_section: false,
			..Default::default()
		};
		let mut results = vec![sect("a", "doc#chunk0", 0.4), sect("b", "doc#chunk1", 0.9)];
		dedup_by_section(&cfg, &mut results);
		assert_eq!(results.len(), 2, "disabled -> no collapse");
	}

	fn ent(id: &str, vector: Vec<f32>, score: f64) -> ScoredEntity {
		ScoredEntity {
			entity: Entity {
				id: id.into(),
				vector: vector.into(),
				..Default::default()
			},
			score,
		}
	}

	#[test]
	fn mmr_runs_and_selects_diverse_over_near_duplicates() {
		let q = vec![1.0, 0.0, 0.0];
		let mut results: Vec<ScoredEntity> = (0..26)
			.map(|i| ent(&format!("dup{i}"), vec![1.0, 0.0, 0.0], 0.9))
			.collect();
		results.push(ent("distinctB", vec![0.0, 1.0, 0.0], 0.5));
		results.push(ent("distinctC", vec![0.0, 0.0, 1.0], 0.5));

		let cfg = RetrievalConfig {
			mmr_enabled: true,
			mmr_lambda: 0.3,
			mmr_pool_size: 50,
			max_deliver_results: 3,
			..Default::default()
		};
		mmr(&cfg, &q, &mut results);

		assert_eq!(results.len(), 3, "MMR must shrink to max_deliver_results");
		let ids: Vec<&str> = results.iter().map(|r| r.entity.id.as_str()).collect();
		assert!(
			ids.contains(&"distinctB"),
			"diverse item B selected: {ids:?}"
		);
		assert!(
			ids.contains(&"distinctC"),
			"diverse item C selected: {ids:?}"
		);
		let dups = ids.iter().filter(|id| id.starts_with("dup")).count();
		assert_eq!(dups, 1, "only one near-duplicate should survive: {ids:?}");
	}

	#[test]
	fn mmr_lambda_one_is_pure_relevance_order() {
		let q = vec![1.0, 0.0, 0.0];
		let mut results = vec![
			ent("c", vec![1.0, 1.0, 0.0], 0.1),
			ent("a", vec![1.0, 0.0, 0.0], 0.1),
			ent("b", vec![0.0, 1.0, 0.0], 0.1),
			ent("e", vec![1.0, 0.1, 0.0], 0.1),
			ent("d", vec![0.0, 0.0, 1.0], 0.1),
		];
		let cfg = RetrievalConfig {
			mmr_enabled: true,
			mmr_lambda: 1.0,
			mmr_pool_size: 50,
			max_deliver_results: 3,
			..Default::default()
		};
		mmr(&cfg, &q, &mut results);
		let ids: Vec<&str> = results.iter().map(|r| r.entity.id.as_str()).collect();
		assert_eq!(
			ids,
			vec!["a", "e", "c"],
			"pure-relevance order, top 3 by cosine"
		);
	}

	// Reference oracle: the pre-optimization O(P*T^2) MMR body. Do NOT simplify — its value is being the old code.
	fn mmr_reference(cfg: &RetrievalConfig, query_vec: &[f32], results: &mut Vec<ScoredEntity>) {
		if !cfg.mmr_enabled || results.len() <= cfg.max_deliver_results {
			return;
		}
		let pool_size = cfg.mmr_pool_size.min(results.len());
		if pool_size == 0 {
			return;
		}
		let target = cfg.max_deliver_results.min(pool_size);
		let lambda = cfg.mmr_lambda;
		let tail = results.split_off(pool_size);
		let mut pool: Vec<ScoredEntity> = std::mem::take(results);
		let mut selected: Vec<ScoredEntity> = Vec::with_capacity(target);
		while selected.len() < target && !pool.is_empty() {
			let mut best_i = 0usize;
			let mut best_score = f64::NEG_INFINITY;
			for (i, cand) in pool.iter().enumerate() {
				let sim_q = if !cand.entity.vector.is_empty() && !query_vec.is_empty() {
					cosine(query_vec, &cand.entity.vector)
				} else {
					cand.score
				};
				let max_sim_selected = selected
					.iter()
					.map(|s| {
						if s.entity.vector.is_empty() || cand.entity.vector.is_empty() {
							0.0
						} else {
							cosine(&s.entity.vector, &cand.entity.vector)
						}
					})
					.fold(0.0_f64, f64::max);
				let mmr_val = lambda * sim_q - (1.0 - lambda) * max_sim_selected;
				if mmr_val > best_score {
					best_score = mmr_val;
					best_i = i;
				}
			}
			selected.push(pool.swap_remove(best_i));
		}
		*results = selected;
		results.extend(tail);
		results.truncate(cfg.max_deliver_results);
	}

	#[test]
	fn mmr_is_byte_identical_to_naive_reference() {
		let mut s: u64 = 0x9E3779B97F4A7C15;
		let mut next = || {
			s ^= s << 13;
			s ^= s >> 7;
			s ^= s << 17;
			s
		};
		let unit = |n: &mut dyn FnMut() -> u64| ((n() % 2001) as f32 - 1000.0) / 1000.0;

		for trial in 0..200u32 {
			let dim = 2 + (next() % 4) as usize;
			let n = 1 + (next() % 40) as usize;
			let q: Vec<f32> = if next() % 11 == 0 {
				Vec::new()
			} else {
				(0..dim).map(|_| unit(&mut next)).collect()
			};
			let mut a: Vec<ScoredEntity> = Vec::with_capacity(n);
			for i in 0..n {
				let vector = if next() % 7 == 0 {
					Vec::new()
				} else {
					(0..dim).map(|_| unit(&mut next)).collect()
				};
				let score = (next() % 3) as f64 / 2.0;
				a.push(ent(&format!("e{i}"), vector, score));
			}
			let cfg = RetrievalConfig {
				mmr_enabled: true,
				mmr_lambda: [0.0, 0.3, 0.45, 0.7, 1.0][(next() % 5) as usize],
				mmr_pool_size: 1 + (next() % 50) as usize,
				max_deliver_results: 1 + (next() % 30) as usize,
				..Default::default()
			};
			let mut b = a.clone();
			mmr(&cfg, &q, &mut a);
			mmr_reference(&cfg, &q, &mut b);
			let ga: Vec<&str> = a.iter().map(|r| r.entity.id.as_str()).collect();
			let gb: Vec<&str> = b.iter().map(|r| r.entity.id.as_str()).collect();
			assert_eq!(
				ga, gb,
				"trial {trial}: optimized mmr diverged from reference"
			);
		}
	}

	#[test]
	fn mmr_noop_when_disabled() {
		let q = vec![1.0, 0.0];
		let mut results: Vec<ScoredEntity> = (0..30)
			.map(|i| ent(&format!("e{i}"), vec![1.0, 0.0], 0.5))
			.collect();
		let cfg = RetrievalConfig {
			mmr_enabled: false,
			..Default::default()
		};
		mmr(&cfg, &q, &mut results);
		assert_eq!(results.len(), 30, "disabled MMR must not touch results");
	}
}
