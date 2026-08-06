use crate::base::constants::{
	KERN_COHESION_THRESHOLD, KERN_NAMING_COHESION_THRESHOLD, KERN_NAMING_MIN_CLUSTER_SIZE,
};
use crate::base::math::cosine;
use crate::base::types::Entity;
use crate::base::util::cmp_partial;
use rayon::prelude::*;

pub struct Cluster {
	pub members: Vec<Entity>,
}

pub fn vector_cluster(thoughts: &[&Entity], max_sample: usize) -> Vec<Cluster> {
	let mut all: Vec<&Entity> = thoughts
		.iter()
		.filter(|t| t.has_vector())
		.copied()
		.collect();
	if all.is_empty() {
		return Vec::new();
	}
	if all.len() > max_sample {
		all.truncate(max_sample);
	}

	let mut assigned = std::collections::HashSet::new();
	let mut clusters = Vec::new();

	for seed in &all {
		if assigned.contains(&seed.id) {
			continue;
		}
		let mut c = Cluster {
			members: vec![(*seed).clone()],
		};
		assigned.insert(seed.id.clone());

		let centroid = &seed.vector;
		let candidates: Vec<usize> = (0..all.len())
			.filter(|&i| !assigned.contains(&all[i].id))
			.collect();
		let scores: Vec<(usize, f64)> = candidates
			.par_iter()
			.map(|&i| (i, cosine(centroid, &all[i].vector)))
			.collect();

		for (i, score) in scores {
			if score >= KERN_COHESION_THRESHOLD && !assigned.contains(&all[i].id) {
				c.members.push(all[i].clone());
				assigned.insert(all[i].id.clone());
			}
		}
		clusters.push(c);
	}
	clusters
}

pub fn largest_cohesive_cluster_for_naming(clusters: &[Cluster]) -> Option<usize> {
	best_cluster(
		clusters,
		KERN_NAMING_MIN_CLUSTER_SIZE,
		KERN_NAMING_COHESION_THRESHOLD,
	)
}

fn best_cluster(clusters: &[Cluster], min_size: usize, min_cohesion: f64) -> Option<usize> {
	let mut best: Option<usize> = None;
	let mut best_size = 0;
	for (i, c) in clusters.iter().enumerate() {
		if c.members.len() >= min_size
			&& cohesion(&c.members) >= min_cohesion
			&& c.members.len() > best_size
		{
			best_size = c.members.len();
			best = Some(i);
		}
	}
	best
}

pub fn is_core_cluster(c: &Cluster, graviton_vec: &[f32]) -> bool {
	if graviton_vec.is_empty() || c.members.is_empty() {
		return false;
	}
	let centroid = compute_centroid(&c.members);
	cosine(&centroid, graviton_vec) >= KERN_COHESION_THRESHOLD
}

pub fn cohesion(members: &[Entity]) -> f64 {
	if members.is_empty() {
		return 0.0;
	}
	let centroid = compute_centroid(members);
	let sum: f64 = members
		.par_iter()
		.map(|m| cosine(&centroid, &m.vector))
		.sum();
	sum / members.len() as f64
}

pub fn centroid_thought(c: &Cluster) -> Option<&Entity> {
	if c.members.is_empty() {
		return None;
	}
	let centroid = compute_centroid(&c.members);
	c.members.iter().max_by(|a, b| {
		cosine(&centroid, &a.vector)
			.partial_cmp(&cosine(&centroid, &b.vector))
			.unwrap_or(std::cmp::Ordering::Equal)
	})
}

pub fn compute_centroid(members: &[Entity]) -> Vec<f32> {
	if members.is_empty() || members[0].vector.is_empty() {
		return Vec::new();
	}
	let dim = members[0].vector.len();
	let mut centroid = vec![0.0; dim];
	for m in members {
		for (i, v) in m.vector.iter().enumerate() {
			if i < dim {
				centroid[i] += v;
			}
		}
	}
	let n = members.len() as f32;
	for v in centroid.iter_mut() {
		*v /= n;
	}
	centroid
}

pub fn graviton_prompt(c: &Cluster) -> String {
	const MAX_SAMPLES: usize = 10;
	let members = if c.members.len() > MAX_SAMPLES {
		let centroid = compute_centroid(&c.members);
		let mut ranked: Vec<(usize, f64)> = c
			.members
			.iter()
			.enumerate()
			.map(|(i, t)| (i, cosine(&centroid, &t.vector)))
			.collect();
		ranked.sort_by(|a, b| cmp_partial(&b.1, &a.1));
		ranked
			.iter()
			.take(MAX_SAMPLES)
			.map(|(i, _)| &c.members[*i])
			.collect::<Vec<_>>()
	} else {
		c.members.iter().collect()
	};

	use std::fmt::Write as _;
	let mut sb = String::from("Summarize the core theme of these related thoughts in one concise phrase. Reply with ONLY the phrase, no prefix, no punctuation at the end:\n\n");
	for t in members {
		let _ = writeln!(sb, "- {}", t.text());
	}
	sb
}

#[cfg(test)]
mod tests {
	use super::*;

	use crate::test_support::entity_vec as ent;

	#[test]
	fn graviton_prompt_keeps_header_then_one_bullet_per_member() {
		let c = Cluster {
			members: vec![
				ent("a", vec![1.0]),
				ent("b", vec![1.0]),
				ent("c", vec![1.0]),
			],
		};
		let p = graviton_prompt(&c);
		assert!(
			p.starts_with("Summarize the core theme of these related thoughts"),
			"instruction header is preserved verbatim",
		);
		assert!(
			p.contains(":\n\n"),
			"blank line separates the header from the list"
		);
		assert_eq!(
			p.matches("\n- ").count(),
			3,
			"exactly one `- ` bullet per member"
		);
		assert!(p.ends_with('\n'), "each bullet line is newline-terminated");
	}

	#[test]
	fn compute_centroid_is_componentwise_mean() {
		let m = vec![ent("a", vec![1.0, 0.0]), ent("b", vec![3.0, 2.0])];
		assert_eq!(compute_centroid(&m), vec![2.0, 1.0]);
	}

	#[test]
	fn compute_centroid_empty_is_empty() {
		assert!(compute_centroid(&[]).is_empty());
	}

	#[test]
	fn cohesion_of_identical_vectors_is_one() {
		let m = vec![ent("a", vec![1.0, 0.0]), ent("b", vec![1.0, 0.0])];
		assert!((cohesion(&m) - 1.0).abs() < 1e-9);
	}

	#[test]
	fn cohesion_empty_is_zero() {
		assert_eq!(cohesion(&[]), 0.0);
	}

	#[test]
	fn vector_cluster_empty_input_yields_no_clusters() {
		assert!(vector_cluster(&[], 100).is_empty());
	}

	#[test]
	fn vector_cluster_identical_vectors_collapse_to_one() {
		let m = [
			ent("a", vec![1.0, 0.0]),
			ent("b", vec![1.0, 0.0]),
			ent("c", vec![1.0, 0.0]),
		];
		let refs: Vec<&Entity> = m.iter().collect();
		let clusters = vector_cluster(&refs, 100);
		assert_eq!(clusters.len(), 1, "identical vectors form a single cluster");
		assert_eq!(clusters[0].members.len(), 3);
	}

	#[test]
	fn vector_cluster_respects_max_sample() {
		let m: Vec<Entity> = (0..5)
			.map(|i| ent(&format!("e{i}"), vec![1.0, 0.0]))
			.collect();
		let refs: Vec<&Entity> = m.iter().collect();
		let clusters = vector_cluster(&refs, 2);
		let total: usize = clusters.iter().map(|c| c.members.len()).sum();
		assert_eq!(total, 2, "only max_sample entities are considered");
	}

	#[test]
	fn centroid_thought_picks_member_in_dominant_direction() {
		let c = Cluster {
			members: vec![
				ent("a", vec![1.0, 0.0]),
				ent("b", vec![1.0, 0.0]),
				ent("c", vec![0.0, 1.0]),
			],
		};
		let rep = centroid_thought(&c).expect("non-empty cluster has a representative");
		assert!(
			rep.vector[0] > rep.vector[1],
			"representative aligns with the dominant direction"
		);
	}

	#[test]
	fn centroid_thought_empty_is_none() {
		assert!(centroid_thought(&Cluster { members: vec![] }).is_none());
	}

	#[test]
	fn is_core_cluster_false_when_graviton_empty() {
		let c = Cluster {
			members: vec![ent("a", vec![1.0, 0.0])],
		};
		assert!(!is_core_cluster(&c, &[]));
	}

	#[test]
	fn best_cluster_prefers_larger_cohesive_cluster() {
		let small = Cluster {
			members: vec![ent("a", vec![1.0, 0.0]), ent("b", vec![1.0, 0.0])],
		};
		let large = Cluster {
			members: vec![
				ent("c", vec![0.0, 1.0]),
				ent("d", vec![0.0, 1.0]),
				ent("e", vec![0.0, 1.0]),
			],
		};
		let clusters = vec![small, large];
		assert_eq!(best_cluster(&clusters, 2, 0.5), Some(1));
	}

	#[test]
	fn best_cluster_none_below_min_size() {
		let clusters = vec![Cluster {
			members: vec![ent("a", vec![1.0, 0.0])],
		}];
		assert_eq!(best_cluster(&clusters, 2, 0.5), None);
	}
}
