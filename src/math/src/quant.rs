//! Vector quantization: f32 embeddings stored as int8 with a per-vector scale
//! (4x smaller, cosine error within rounding) or kept f32 — the mode is
//! stamped into the store and must match at open.

use serde::{Deserialize, Serialize};

const INT8_MAX_ABS: f32 = 127.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[repr(u8)]
pub enum QuantizationMode {
	#[default]
	None = 0,
	Int8 = 1,
	/// In-memory only — the on-disk projection (`StoredVec`) stays int8.
	Binary = 2,
}

impl QuantizationMode {
	pub fn parse(s: &str) -> Option<Self> {
		match s.trim().to_ascii_lowercase().as_str() {
			"none" | "f32" | "f64" | "off" => Some(Self::None),
			"int8" | "i8" => Some(Self::Int8),
			// Binary deliberately not user-selectable: recall floor too low without
			// rescore (see `binary_recall_tracks_f64`).
			_ => None,
		}
	}

	pub fn as_str(self) -> &'static str {
		match self {
			Self::None => "none",
			Self::Int8 => "int8",
			Self::Binary => "binary",
		}
	}

	pub fn bytes_per_dim(self) -> f32 {
		match self {
			Self::None => 4.0,
			Self::Int8 => 1.0,
			Self::Binary => 0.125,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuantizedVec {
	pub mode: QuantizationMode,
	pub scale: f32,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub f: Vec<f32>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub q: Vec<i8>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub b: Vec<u8>,
	// True Binary dim: the padded last byte makes `b.len() * 8` over-count.
	#[serde(default)]
	pub dim_bits: usize,
}

impl QuantizedVec {
	pub fn encode(v: &[f32], mode: QuantizationMode) -> Self {
		match mode {
			QuantizationMode::None => Self {
				mode,
				scale: 0.0,
				f: v.to_vec(),
				q: Vec::new(),
				b: Vec::new(),
				dim_bits: 0,
			},
			QuantizationMode::Int8 => encode_int8(v),
			QuantizationMode::Binary => encode_binary(v),
		}
	}

	pub fn decode(&self) -> Vec<f32> {
		match self.mode {
			QuantizationMode::None => self.f.clone(),
			QuantizationMode::Int8 => self.q.iter().map(|&qi| (qi as f32) * self.scale).collect(),
			QuantizationMode::Binary => (0..self.dim_bits)
				.map(|i| {
					if self.b[i / 8] & (1 << (i % 8)) != 0 {
						1.0
					} else {
						-1.0
					}
				})
				.collect(),
		}
	}
}

fn encode_int8(v: &[f32]) -> QuantizedVec {
	if v.is_empty() {
		return QuantizedVec {
			mode: QuantizationMode::Int8,
			scale: 0.0,
			f: Vec::new(),
			q: Vec::new(),
			b: Vec::new(),
			dim_bits: 0,
		};
	}
	let max_abs = v.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
	let scale = if max_abs == 0.0 {
		1.0_f32
	} else {
		max_abs / INT8_MAX_ABS
	};
	let inv = 1.0_f32 / scale;
	let q: Vec<i8> = v
		.iter()
		.map(|&x| {
			let scaled = x * inv;
			let rounded = scaled.round();
			rounded.clamp(-INT8_MAX_ABS, INT8_MAX_ABS) as i8
		})
		.collect();
	QuantizedVec {
		mode: QuantizationMode::Int8,
		scale,
		f: Vec::new(),
		q,
		b: Vec::new(),
		dim_bits: 0,
	}
}

fn encode_binary(v: &[f32]) -> QuantizedVec {
	let mut b = vec![0u8; v.len().div_ceil(8)];
	for (i, &x) in v.iter().enumerate() {
		if x >= 0.0 {
			b[i / 8] |= 1 << (i % 8);
		}
	}
	QuantizedVec {
		mode: QuantizationMode::Binary,
		scale: 0.0,
		f: Vec::new(),
		q: Vec::new(),
		b,
		dim_bits: v.len(),
	}
}

fn binary_cosine_distance(a: &QuantizedVec, b: &QuantizedVec) -> f64 {
	let dim = a.dim_bits.min(b.dim_bits);
	if dim == 0 || a.b.len() != b.b.len() {
		return 1.0;
	}
	let hamming: u32 = a
		.b
		.iter()
		.zip(&b.b)
		.map(|(x, y)| (x ^ y).count_ones())
		.sum();
	let theta = std::f64::consts::PI * (hamming as f64) / (dim as f64);
	1.0 - theta.cos()
}

pub fn quantized_cosine_distance(a: &QuantizedVec, b: &QuantizedVec) -> f64 {
	match (a.mode, b.mode) {
		(QuantizationMode::Int8, QuantizationMode::Int8) => int8_cosine_distance(&a.q, &b.q) as f64,
		(QuantizationMode::Binary, QuantizationMode::Binary) => binary_cosine_distance(a, b),
		_ => {
			let av = a.decode();
			let bv = b.decode();
			float_cosine_distance(&av, &bv)
		}
	}
}

fn float_cosine_distance(a: &[f32], b: &[f32]) -> f64 {
	if a.is_empty() || b.is_empty() || a.len() != b.len() {
		return 1.0;
	}
	1.0 - crate::math::cosine(a, b)
}

fn int8_cosine_distance(a: &[i8], b: &[i8]) -> f32 {
	let n = a.len();
	if n == 0 || n != b.len() {
		return 1.0;
	}
	let (dot, na, nb) = int8_dot_norms(a, b);
	if na == 0 || nb == 0 {
		return 1.0;
	}
	let denom = ((na as f32) * (nb as f32)).sqrt();
	let cos = ((dot as f32) / denom).clamp(-1.0, 1.0);
	1.0 - cos
}

fn int8_dot_norms(a: &[i8], b: &[i8]) -> (i32, i32, i32) {
	#[cfg(target_arch = "x86_64")]
	{
		if is_x86_feature_detected!("avx2") {
			return unsafe { int8_dot_norms_avx2(a, b) };
		}
	}
	int8_dot_norms_scalar(a, b)
}

fn int8_dot_norms_scalar(a: &[i8], b: &[i8]) -> (i32, i32, i32) {
	let (mut dot, mut na, mut nb) = (0i32, 0i32, 0i32);
	for (&ai, &bi) in a.iter().zip(b.iter()) {
		let (ai, bi) = (ai as i32, bi as i32);
		dot += ai * bi;
		na += ai * ai;
		nb += bi * bi;
	}
	(dot, na, nb)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn int8_dot_norms_avx2(a: &[i8], b: &[i8]) -> (i32, i32, i32) {
	use std::arch::x86_64::*;

	// SAFETY INVARIANT: callers pass equal-length slices, so `n = a.len() = b.len()`.
	// `chunks = n / 16`, `tail = chunks * 16`. Each iteration loads 16 bytes at
	// `off = i*16` where `off + 16 <= chunks*16 = tail <= n`, staying within both
	// slices. The scalar tail loop indexes `tail..n`, all `< n <= len`, so every
	// `get_unchecked` is in bounds. `cvtepi8_epi16` sign-extends the 16 i8 lanes to
	// i16; `madd_epi16` multiplies signed i16 pairwise into i32 (max |lane| = 128,
	// pair sum <= 32768) and we accumulate into i32 lanes — the same values and
	// range as the scalar reference, so results match exactly.
	let n = a.len();
	let chunks = n / 16;

	let mut vdot = _mm256_setzero_si256();
	let mut vna = _mm256_setzero_si256();
	let mut vnb = _mm256_setzero_si256();

	let pa = a.as_ptr();
	let pb = b.as_ptr();

	for i in 0..chunks {
		let off = i * 16;
		let a8 = _mm_loadu_si128(pa.add(off) as *const __m128i);
		let b8 = _mm_loadu_si128(pb.add(off) as *const __m128i);
		let a16 = _mm256_cvtepi8_epi16(a8);
		let b16 = _mm256_cvtepi8_epi16(b8);
		vdot = _mm256_add_epi32(vdot, _mm256_madd_epi16(a16, b16));
		vna = _mm256_add_epi32(vna, _mm256_madd_epi16(a16, a16));
		vnb = _mm256_add_epi32(vnb, _mm256_madd_epi16(b16, b16));
	}

	let mut dot = hsum_256_epi32(vdot);
	let mut na = hsum_256_epi32(vna);
	let mut nb = hsum_256_epi32(vnb);

	let tail = chunks * 16;
	for i in tail..n {
		let ai = *a.get_unchecked(i) as i32;
		let bi = *b.get_unchecked(i) as i32;
		dot += ai * bi;
		na += ai * ai;
		nb += bi * bi;
	}
	(dot, na, nb)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hsum_256_epi32(v: std::arch::x86_64::__m256i) -> i32 {
	use std::arch::x86_64::*;
	let hi = _mm256_extracti128_si256(v, 1);
	let lo = _mm256_castsi256_si128(v);
	let sum128 = _mm_add_epi32(lo, hi);
	let hi64 = _mm_unpackhi_epi64(sum128, sum128);
	let sum64 = _mm_add_epi32(sum128, hi64);
	let hi32 = _mm_shuffle_epi32(sum64, 0b01);
	let sum32 = _mm_add_epi32(sum64, hi32);
	_mm_cvtsi128_si32(sum32)
}

#[cfg(test)]
#[path = "tests/quant_test.rs"]
mod quant_tests;
