//! Weighted reciprocal-rank fusion: merge several ranked hit lists into one by
//! summing `weight / (k + rank)` per entity — rank-based, so lists whose raw
//! scores are not comparable (content vs GNN vs lexical) can still be fused.

use crate::search::EntityHit;
use std::collections::HashMap;

pub fn rrf(lists: &[&[EntityHit]], weights: &[f64], k_rrf: f64, top_k: usize) -> Vec<EntityHit> {
	let mut agg: HashMap<String, f64> = HashMap::new();
	for (li, list) in lists.iter().enumerate() {
		let w = weights.get(li).copied().unwrap_or(1.0);
		for (i, hit) in list.iter().enumerate() {
			let rank = (i + 1) as f64;
			let contrib = w / (k_rrf + rank);
			*agg.entry(hit.entity_id.clone()).or_insert(0.0) += contrib;
		}
	}
	if top_k == 0 {
		return Vec::new();
	}
	let mut out: Vec<EntityHit> = agg.into_iter().map(EntityHit::from).collect();
	// Unique ids make this a STRICT total order, so the top_k partition + sorting only the survivors equals a full sort + truncate.
	let cmp = |a: &EntityHit, b: &EntityHit| {
		crate::util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id)
	};
	if top_k < out.len() {
		out.select_nth_unstable_by(top_k - 1, &cmp);
		out.truncate(top_k);
	}
	out.sort_by(&cmp);
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn hit(id: &str) -> EntityHit {
		EntityHit {
			entity_id: id.into(),
			score: 0.0,
		}
	}

	#[test]
	fn empty_weights_recovers_unweighted_rrf() {
		let a = [hit("x"), hit("y")];
		let b = [hit("y"), hit("z")];
		let lists: Vec<&[EntityHit]> = vec![&a, &b];
		let out = rrf(&lists, &[], 60.0, 10);
		assert_eq!(out[0].entity_id, "y", "y in both lists sorts first");
	}

	#[test]
	fn global_list_downweight_sinks_popular_irrelevant_entity() {
		let dense = [hit("rel")];
		let global = [hit("pop")];
		let lists: Vec<&[EntityHit]> = vec![&dense, &global];

		let unweighted = rrf(&lists, &[1.0, 1.0], 60.0, 10);
		assert_eq!(unweighted[0].entity_id, "pop", "equal weights: id tiebreak");

		let weighted = rrf(&lists, &[1.0, 0.5], 60.0, 10);
		assert_eq!(weighted[0].entity_id, "rel", "down-weighted global sinks");
		assert!(
			weighted[0].score > weighted[1].score,
			"rel strictly above pop"
		);
	}

	#[test]
	fn missing_weight_defaults_to_one() {
		let a = [hit("x")];
		let b = [hit("x")];
		let lists: Vec<&[EntityHit]> = vec![&a, &b];
		let out = rrf(&lists, &[1.0], 60.0, 10);
		let both = rrf(&lists, &[1.0, 1.0], 60.0, 10);
		assert_eq!(out[0].score, both[0].score, "missing weight == 1.0");
	}

	#[test]
	fn equal_score_tie_broken_by_id_ascending_under_top_k() {
		let la = [hit("b")];
		let lb = [hit("a")];
		let lists: Vec<&[EntityHit]> = vec![&la, &lb];
		let out = rrf(&lists, &[1.0, 1.0], 60.0, 1);
		assert_eq!(out.len(), 1, "top_k=1 keeps a single hit");
		assert_eq!(
			out[0].entity_id, "a",
			"tie resolved to id-ascending winner under truncation"
		);
	}

	#[test]
	fn top_k_truncates_and_zero_is_empty_without_panicking() {
		let a = [hit("x"), hit("y"), hit("z")];
		let lists: Vec<&[EntityHit]> = vec![&a];

		assert!(rrf(&lists, &[], 60.0, 0).is_empty(), "top_k=0 is empty");
		assert_eq!(rrf(&lists, &[], 60.0, 2).len(), 2, "truncates to top_k");
		assert_eq!(
			rrf(&lists, &[], 60.0, 99).len(),
			3,
			"top_k over count returns all"
		);
	}
}
