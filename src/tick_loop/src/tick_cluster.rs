//! Greedy cosine clustering of a kern's entities, sampled and parallelised —
//! the shape detector behind kern splitting and naming: a cohesive-enough
//! cluster earns a name; the core cluster stays with its graviton.

use base::base_constants::{
	KERN_COHESION_THRESHOLD, KERN_NAMING_COHESION_THRESHOLD, KERN_NAMING_MIN_CLUSTER_SIZE,
};
use base::base_types::Entity;
use math::cosine;
use rayon::prelude::*;
use util::cmp_partial;

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
#[path = "tests/tick_cluster_test.rs"]
mod tick_cluster_tests;
