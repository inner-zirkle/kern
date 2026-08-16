//! Tests extracted from quant.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn int8_round_trip_within_scale() {
		let v = vec![1.0f32, -2.0, 0.5, 0.0, -0.25];
		let qv = QuantizedVec::encode(&v, QuantizationMode::Int8);
		let d = qv.decode();
		assert_eq!(d.len(), v.len());
		for (orig, got) in v.iter().zip(&d) {
			assert!(
				(orig - got).abs() <= qv.scale + 1e-6,
				"{orig} vs {got} (scale {})",
				qv.scale
			);
		}
	}

	#[test]
	fn none_mode_is_lossless() {
		let v = vec![1.5f32, -0.3, 9.0];
		let qv = QuantizedVec::encode(&v, QuantizationMode::None);
		assert_eq!(qv.decode(), v);
	}

	#[test]
	fn empty_and_zero_vectors() {
		let empty = QuantizedVec::encode(&[], QuantizationMode::Int8);
		assert!(empty.q.is_empty());
		assert!(empty.decode().is_empty());

		let zero = QuantizedVec::encode(&[0.0, 0.0, 0.0], QuantizationMode::Int8);
		assert!(zero.q.iter().all(|&q| q == 0));
		assert_eq!(zero.decode(), vec![0.0, 0.0, 0.0]);
	}

	#[test]
	fn int8_cosine_identical_is_zero_orthogonal_is_one() {
		let a = QuantizedVec::encode(&[1.0, 2.0, 3.0], QuantizationMode::Int8);
		let b = QuantizedVec::encode(&[1.0, 2.0, 3.0], QuantizationMode::Int8);
		assert!(quantized_cosine_distance(&a, &b) < 1e-3);

		let x = QuantizedVec::encode(&[1.0, 0.0], QuantizationMode::Int8);
		let y = QuantizedVec::encode(&[0.0, 1.0], QuantizationMode::Int8);
		assert!((quantized_cosine_distance(&x, &y) - 1.0).abs() < 1e-3);
	}

	#[test]
	fn mixed_mode_falls_back_to_decoded_float() {
		let a = QuantizedVec::encode(&[1.0, 2.0, 3.0], QuantizationMode::Int8);
		let b = QuantizedVec::encode(&[1.0, 2.0, 3.0], QuantizationMode::None);
		assert!(quantized_cosine_distance(&a, &b) < 1e-2);
	}

	#[test]
	fn mixed_mode_exactly_matches_the_decoded_float_distance() {
		let int8 = QuantizedVec::encode(&[1.0, -2.0, 3.0, 0.5], QuantizationMode::Int8);
		let none = QuantizedVec::encode(&[1.0, -2.0, 3.0, 0.5], QuantizationMode::None);
		let expected = float_cosine_distance(&int8.decode(), &none.decode());

		assert_eq!(
			quantized_cosine_distance(&int8, &none),
			expected,
			"int8 vs none == decoded float"
		);
		assert_eq!(
			quantized_cosine_distance(&none, &int8),
			expected,
			"none vs int8 is symmetric"
		);
	}

	#[test]
	fn float_cosine_edge_cases() {
		assert_eq!(float_cosine_distance(&[], &[]), 1.0);
		assert_eq!(float_cosine_distance(&[1.0, 2.0], &[1.0]), 1.0);
		assert_eq!(float_cosine_distance(&[0.0, 0.0], &[1.0, 1.0]), 1.0);
		assert!(float_cosine_distance(&[1.0, 1.0], &[1.0, 1.0]) < 1e-6);
	}

	#[test]
	fn mode_parse_round_trip() {
		assert_eq!(
			QuantizationMode::parse("int8"),
			Some(QuantizationMode::Int8)
		);
		assert_eq!(
			QuantizationMode::parse(" NONE "),
			Some(QuantizationMode::None)
		);
		assert_eq!(QuantizationMode::parse("bogus"), None);
		assert_eq!(QuantizationMode::Int8.as_str(), "int8");
		assert_eq!(
			QuantizationMode::parse("binary"),
			None,
			"not config-exposed until rescore"
		);
		assert_eq!(QuantizationMode::Binary.as_str(), "binary");
		assert_eq!(QuantizationMode::Binary.bytes_per_dim(), 0.125);
	}

	#[test]
	fn binary_packs_one_sign_bit_per_dim() {
		let v = vec![1.0f32, -1.0, 0.0, -0.5, 2.0, -3.0, 0.1, -0.1, 5.0, -5.0];
		let qv = QuantizedVec::encode(&v, QuantizationMode::Binary);
		assert_eq!(
			qv.dim_bits, 10,
			"dim_bits is the true dimension, not b.len()*8"
		);
		assert_eq!(qv.b.len(), 2, "10 dims pack into ceil(10/8)=2 bytes");
		assert_eq!(qv.b[0], 0b0101_0101, "bit i set iff v[i] >= 0");
		assert_eq!(qv.b[1], 0b0000_0001, "high byte: only dim 8 (>=0) set");
	}

	#[test]
	fn binary_decode_reconstructs_signs() {
		let v = vec![3.0f32, -2.0, 0.0, -7.0];
		let qv = QuantizedVec::encode(&v, QuantizationMode::Binary);
		assert_eq!(
			qv.decode(),
			vec![1.0, -1.0, 1.0, -1.0],
			"0.0 counts as + (>=0)"
		);
	}

	#[test]
	fn binary_distance_zero_for_identical_and_monotone_in_angle() {
		let a = QuantizedVec::encode(&[1.0, 1.0, 1.0, 1.0], QuantizationMode::Binary);
		let b = QuantizedVec::encode(&[1.0, 1.0, 1.0, 1.0], QuantizationMode::Binary);
		assert!(
			quantized_cosine_distance(&a, &b).abs() < 1e-12,
			"identical signs -> 0"
		);

		let c = QuantizedVec::encode(&[-1.0, -1.0, -1.0, -1.0], QuantizationMode::Binary);
		assert!(
			(quantized_cosine_distance(&a, &c) - 2.0).abs() < 1e-12,
			"all bits differ -> 2"
		);

		let d = QuantizedVec::encode(&[1.0, 1.0, -1.0, -1.0], QuantizationMode::Binary);
		assert!(
			(quantized_cosine_distance(&a, &d) - 1.0).abs() < 1e-12,
			"half differ -> 1"
		);
	}

	#[cfg(target_arch = "x86_64")]
	#[test]
	fn int8_avx2_dot_norms_match_scalar_reference() {
		if !is_x86_feature_detected!("avx2") {
			return;
		}
		let mut state = 0x2545_f491_4f6c_dd1d_u64;
		let mut next_i8 = || {
			state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
			(state >> 33) as i8
		};
		for &len in &[0usize, 1, 7, 15, 16, 17, 31, 33, 64, 100] {
			let a: Vec<i8> = (0..len).map(|_| next_i8()).collect();
			let b: Vec<i8> = (0..len).map(|_| next_i8()).collect();
			let scalar = int8_dot_norms_scalar(&a, &b);
			// SAFETY: guarded by the runtime avx2 feature check above; a.len()==b.len().
			let simd = unsafe { int8_dot_norms_avx2(&a, &b) };
			assert_eq!(
				scalar, simd,
				"len {len}: avx2 {simd:?} vs scalar {scalar:?}"
			);
		}
		for pattern in [
			vec![127i8; 20],
			vec![-128i8; 20],
			(0..20)
				.map(|i| if i % 2 == 0 { 127 } else { -128 })
				.collect(),
		] {
			let scalar = int8_dot_norms_scalar(&pattern, &pattern);
			// SAFETY: avx2 checked above; equal-length inputs.
			let simd = unsafe { int8_dot_norms_avx2(&pattern, &pattern) };
			assert_eq!(
				scalar, simd,
				"extreme lanes: avx2 {simd:?} vs scalar {scalar:?}"
			);
		}
	}

	#[test]
	fn binary_hamming_ranking_tracks_true_cosine() {
		let query = vec![1.0f32, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];
		let near = vec![1.0f32, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, -1.0];
		let far = vec![-1.0f32, -1.0, -1.0, -1.0, 1.0, 1.0, 1.0, 1.0];
		let q = QuantizedVec::encode(&query, QuantizationMode::Binary);
		let n = QuantizedVec::encode(&near, QuantizationMode::Binary);
		let f = QuantizedVec::encode(&far, QuantizationMode::Binary);
		assert!(
			quantized_cosine_distance(&q, &n) < quantized_cosine_distance(&q, &f),
			"fewer sign flips -> smaller Hamming distance"
		);
	}
}
