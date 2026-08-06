use crate::accept::root_graviton_ids;
use crate::graph::GraphGnn;
use crate::math::cosine;
use crate::base_types::Kern;
use crate::config::RetrievalConfig;
use crate::retrieval::expand::Scored;

// Max over gravitons, not sum — overlapping gravitons must not double-count.
pub fn apply_gravity<T: Scored>(g: &GraphGnn, cfg: &RetrievalConfig, results: &mut [T]) {
	if cfg.gravity_weight == 0.0 {
		return;
	}
	let gravitons: Vec<&Kern> = root_graviton_ids(g)
		.into_iter()
		.filter_map(|id| g.loaded(&id))
		.filter(|k| !k.graviton_vec.is_empty())
		.collect();
	if gravitons.is_empty() {
		return;
	}
	for r in results.iter_mut() {
		let vec = &r.entity().vector;
		if vec.is_empty() {
			continue;
		}
		let pull = gravitons
			.iter()
			.map(|k| k.mass * cosine(&k.graviton_vec, vec).max(0.0))
			.fold(0.0_f64, f64::max);
		if pull > 0.0 {
			r.set_score(r.score() + cfg.gravity_weight * pull);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::accept::add_graviton_with_mass;
	use crate::base_types::{mk_entity, EntityKind};
	use crate::retrieval::expand::ScoredEntity;

	fn scored(id: &str, vector: Vec<f32>, score: f64) -> ScoredEntity {
		let mut entity = mk_entity(id, "t", 0.5, EntityKind::Claim);
		entity.vector = vector.into();
		ScoredEntity { entity, score }
	}

	fn graph_with_graviton(mass: f64) -> GraphGnn {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		let id = root_graviton_ids(&g).pop().unwrap();
		g.get_mut(&id).unwrap().mass = mass;
		g
	}

	#[test]
	fn graviton_near_entity_outranks_graviton_far_at_equal_base_score() {
		let g = graph_with_graviton(1.0);
		let cfg = RetrievalConfig::default();
		let mut results = vec![
			scored("far", vec![0.0, 1.0, 0.0], 1.0),
			scored("near", vec![1.0, 0.0, 0.0], 1.0),
			scored("novec", Vec::new(), 1.0),
		];
		apply_gravity(&g, &cfg, &mut results);
		let get = |id: &str| results.iter().find(|r| r.entity.id == id).unwrap().score;
		assert!(
			get("near") > get("far"),
			"near {} must outrank far {}",
			get("near"),
			get("far")
		);
		assert_eq!(get("far"), 1.0, "orthogonal cosine -> no boost");
		assert_eq!(get("novec"), 1.0, "empty entity vector is skipped");
	}

	#[test]
	fn mass_two_pulls_harder_than_mass_one() {
		let cfg = RetrievalConfig::default();
		let boost = |mass: f64| {
			let g = graph_with_graviton(mass);
			let mut results = vec![scored("e", vec![1.0, 0.0, 0.0], 1.0)];
			apply_gravity(&g, &cfg, &mut results);
			results[0].score - 1.0
		};
		let (b1, b2) = (boost(1.0), boost(2.0));
		assert!(b1 > 0.0, "mass 1 boosts at all: {b1}");
		assert!(
			(b2 - 2.0 * b1).abs() < 1e-9,
			"mass scales the pull linearly: {b2} vs 2*{b1}"
		);
	}

	#[test]
	fn gravity_weight_zero_changes_nothing() {
		let g = graph_with_graviton(1.0);
		let cfg = RetrievalConfig {
			gravity_weight: 0.0,
			..Default::default()
		};
		let mut results = vec![scored("near", vec![1.0, 0.0, 0.0], 1.0)];
		apply_gravity(&g, &cfg, &mut results);
		assert_eq!(results[0].score, 1.0);
	}

	#[test]
	fn overlapping_gravitons_take_the_max_not_the_sum() {
		let mut g = graph_with_graviton(1.0);
		add_graviton_with_mass(&mut g, "also-work", vec![1.0, 0.0, 0.0], 1.0);
		let cfg = RetrievalConfig::default();
		let mut results = vec![scored("e", vec![1.0, 0.0, 0.0], 1.0)];
		apply_gravity(&g, &cfg, &mut results);
		let boost = results[0].score - 1.0;
		assert!(
			(boost - cfg.gravity_weight).abs() < 1e-6,
			"two identical unit gravitons boost once, got {boost}"
		);
	}
}
