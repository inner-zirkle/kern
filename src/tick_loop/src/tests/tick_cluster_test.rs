//! Tests extracted from tick_cluster.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	use test_support::entity_vec as ent;

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
