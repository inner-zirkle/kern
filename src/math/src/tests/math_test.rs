//! Tests extracted from math.rs
#![allow(unused)]
use super::*;

mod cosine_tests {
	use super::*;

	#[test]
	fn identical_vectors_are_one_orthogonal_are_zero() {
		assert!((cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-6);
		assert!(
			cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6,
			"orthogonal -> 0"
		);
	}

	#[test]
	fn zero_norm_inputs_return_zero_not_nan() {
		assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
		assert_eq!(cosine(&[1.0, 1.0], &[0.0, 0.0]), 0.0);
		assert_eq!(cosine(&[0.0, 0.0], &[0.0, 0.0]), 0.0);
	}

	#[test]
	fn mismatched_lengths_compare_the_shared_prefix() {
		let c = cosine(&[1.0, 0.0, 9.0], &[1.0, 0.0]);
		assert!(
			(c - 1.0).abs() < 1e-6,
			"shared prefix is identical -> 1.0, got {c}"
		);
		assert_eq!(cosine(&[], &[1.0, 2.0]), 0.0);
	}

	// Lengths exercise both the 8-wide chunk loop and the unchecked tail (17 = 2*8+1).
	#[cfg(target_arch = "x86_64")]
	#[test]
	fn avx2_path_matches_scalar_reference() {
		if !(is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma")) {
			return; // no SIMD on this host; scalar already covered above
		}
		for len in [0usize, 1, 7, 8, 9, 15, 16, 17, 33, 100] {
			let a: Vec<f32> = (0..len).map(|i| i as f32 * 0.1 - 0.5).collect();
			let b: Vec<f32> = (0..len).map(|i| (len - i) as f32 * 0.2 + 0.3).collect();
			let scalar = cosine_scalar(&a, &b);
			// SAFETY: guarded by the runtime avx2+fma feature check above.
			let simd = unsafe { cosine_avx2(&a, &b) };
			assert!(
				(scalar - simd).abs() < 1e-5,
				"len {len}: avx2 {simd} vs scalar {scalar}"
			);
		}
	}
}
mod l2_normalize_tests {
	use super::l2_normalize;

	#[test]
	fn scales_to_unit_norm() {
		let mut v = vec![3.0f32, 4.0];
		l2_normalize(&mut v);
		assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
		let norm = v
			.iter()
			.map(|&x| (x as f64) * (x as f64))
			.sum::<f64>()
			.sqrt();
		assert!((norm - 1.0).abs() < 1e-6);
	}

	#[test]
	fn zero_vector_is_left_unchanged() {
		let mut v = vec![0.0f32, 0.0, 0.0];
		l2_normalize(&mut v);
		assert_eq!(v, vec![0.0, 0.0, 0.0], "no divide-by-zero / NaN");
	}

	#[test]
	fn empty_slice_is_a_noop() {
		let mut v: Vec<f32> = vec![];
		l2_normalize(&mut v);
		assert!(v.is_empty());
	}
}
mod online_softmax_tests {
	use super::OnlineSoftmax;

	#[test]
	fn empty_finalizes_to_neg_infinity() {
		assert_eq!(OnlineSoftmax::new().finalize(), f64::NEG_INFINITY);
	}

	#[test]
	fn single_observation_is_identity() {
		let mut s = OnlineSoftmax::new();
		s.update(0.7);
		assert!((s.finalize() - 0.7).abs() < 1e-12);
	}

	#[test]
	fn two_equal_observations_add_ln2() {
		let mut s = OnlineSoftmax::new();
		s.update(0.5);
		s.update(0.5);
		assert!((s.finalize() - (0.5 + 2.0_f64.ln())).abs() < 1e-12);
	}

	#[test]
	fn corroborated_item_can_outrank_higher_single_observation() {
		// Pins the pooling design — a switch to running_max is a deliberate, test-breaking change.
		let mut corroborated = OnlineSoftmax::new();
		corroborated.update(0.8);
		corroborated.update(0.8);
		let mut single = OnlineSoftmax::new();
		single.update(0.9);
		assert!(corroborated.finalize() > single.finalize());
		assert!(corroborated.running_max() < single.running_max());
	}
}
