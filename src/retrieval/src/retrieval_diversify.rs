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
#[path = "tests/retrieval_diversify_test.rs"]
mod retrieval_diversify_tests;
