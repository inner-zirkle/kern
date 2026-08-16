//! Vector and scoring math: SIMD-pathed cosine, vector averaging, online
//! softmax, importance and radius scoring, and the deterministic reason-id
//! mint — pure functions, no graph state.

use base::base_constants::*;
use base::base_types::{EntityKind, ReasonKind};

pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
	#[cfg(target_arch = "x86_64")]
	{
		if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
			return unsafe { cosine_avx2(a, b) };
		}
	}
	cosine_scalar(a, b)
}

fn cosine_scalar(a: &[f32], b: &[f32]) -> f64 {
	let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
	for (ai, bi) in a.iter().zip(b.iter()) {
		dot += ai * bi;
		na += ai * ai;
		nb += bi * bi;
	}
	if na == 0.0 || nb == 0.0 {
		return 0.0;
	}
	(dot as f64) / ((na as f64).sqrt() * (nb as f64).sqrt())
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn cosine_avx2(a: &[f32], b: &[f32]) -> f64 {
	use std::arch::x86_64::*;

	// SAFETY INVARIANT for every unchecked access below: `n = min(a.len, b.len)`,
	// `chunks = n / 8`, `rem = n % 8`, `tail = chunks * 8`. Therefore:
	//  - the loaded chunks span offsets `0..tail` and each `loadu_ps` reads 8
	//    lanes at `off = i*8` where `off + 8 <= chunks*8 = tail <= n`, so it stays
	//    within both slices (`tail <= a.len()` and `tail <= b.len()`);
	//  - the tail loop indexes `tail + i` for `i in 0..rem`, and
	//    `tail + rem = chunks*8 + n%8 = n <= a.len()` (and `<= b.len()`),
	//    so `get_unchecked(tail + i)` is always in bounds.
	let n = a.len().min(b.len());
	let chunks = n / 8;
	let rem = n % 8;

	let mut vdot = _mm256_setzero_ps();
	let mut vna = _mm256_setzero_ps();
	let mut vnb = _mm256_setzero_ps();

	let pa = a.as_ptr();
	let pb = b.as_ptr();

	for i in 0..chunks {
		let off = i * 8;
		// In bounds: off + 8 <= chunks*8 = tail <= n <= len of both slices.
		let va = _mm256_loadu_ps(pa.add(off));
		let vb = _mm256_loadu_ps(pb.add(off));
		vdot = _mm256_fmadd_ps(va, vb, vdot);
		vna = _mm256_fmadd_ps(va, va, vna);
		vnb = _mm256_fmadd_ps(vb, vb, vnb);
	}

	let mut dot = hsum_256_ps(vdot);
	let mut na = hsum_256_ps(vna);
	let mut nb = hsum_256_ps(vnb);

	let tail = chunks * 8;
	for i in 0..rem {
		// In bounds: tail + i < tail + rem = n <= len of both slices.
		let ai = *a.get_unchecked(tail + i);
		let bi = *b.get_unchecked(tail + i);
		dot += ai * bi;
		na += ai * ai;
		nb += bi * bi;
	}

	if na == 0.0 || nb == 0.0 {
		return 0.0;
	}
	(dot as f64) / ((na as f64).sqrt() * (nb as f64).sqrt())
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn hsum_256_ps(v: std::arch::x86_64::__m256) -> f32 {
	use std::arch::x86_64::*;
	let high = _mm256_extractf128_ps(v, 1);
	let low = _mm256_castps256_ps128(v);
	let sum128 = _mm_add_ps(low, high);
	let hi64 = _mm_movehl_ps(sum128, sum128);
	let sum64 = _mm_add_ps(sum128, hi64);
	let hi32 = _mm_shuffle_ps(sum64, sum64, 0b01);
	let total = _mm_add_ss(sum64, hi32);
	_mm_cvtss_f32(total)
}

pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
	1.0 - cosine(a, b)
}

pub fn average_vec(a: &[f32], b: &[f32]) -> Vec<f32> {
	a.iter()
		.zip(b.iter())
		.map(|(ai, bi)| (ai + bi) / 2.0)
		.collect()
}

// A zero vector (norm 0) is left unchanged — avoids divide-by-zero NaNs.
pub fn l2_normalize(v: &mut [f32]) {
	let norm = v
		.iter()
		.map(|&x| (x as f64) * (x as f64))
		.sum::<f64>()
		.sqrt() as f32;
	if norm > 0.0 {
		for x in v.iter_mut() {
			*x /= norm;
		}
	}
}

pub fn reason_id(from: &str, to: &str, kind: ReasonKind, text: &str) -> String {
	util::content_hash(&format!(
		"{}\x00{}\x00{}\x00{}",
		from, to, kind as i32, text
	))
}

#[derive(Debug, Clone, Copy)]
pub struct OnlineSoftmax {
	m: f64,
	s: f64,
}

impl Default for OnlineSoftmax {
	fn default() -> Self {
		Self::new()
	}
}

impl OnlineSoftmax {
	pub fn new() -> Self {
		Self {
			m: f64::NEG_INFINITY,
			s: 0.0,
		}
	}

	pub fn update(&mut self, x: f64) {
		if !x.is_finite() {
			return;
		}
		let m_new = self.m.max(x);
		let carry = if self.m.is_finite() {
			self.s * (self.m - m_new).exp()
		} else {
			0.0
		};
		self.s = carry + (x - m_new).exp();
		self.m = m_new;
	}

	pub fn is_empty(&self) -> bool {
		self.s == 0.0 && !self.m.is_finite()
	}

	#[cfg(test)]
	fn running_max(&self) -> f64 {
		self.m
	}

	// Deliberately pooling (log-sum-exp), not max — do NOT swap for running_max.
	pub fn finalize(&self) -> f64 {
		if self.is_empty() {
			return f64::NEG_INFINITY;
		}
		self.m + self.s.ln()
	}
}

pub fn softmax_merge_scores<I, K>(iter: I) -> std::collections::HashMap<K, f64>
where
	I: IntoIterator<Item = (K, f64)>,
	K: std::hash::Hash + Eq,
{
	let mut acc: std::collections::HashMap<K, OnlineSoftmax> = std::collections::HashMap::new();
	for (k, v) in iter {
		acc.entry(k).or_default().update(v);
	}
	acc.into_iter().map(|(k, s)| (k, s.finalize())).collect()
}

pub fn clamp_confidence(conf: f64, source: &str) -> (f64, EntityKind) {
	let mut conf = if conf <= 0.0 {
		DEFAULT_CONFIDENCE
	} else {
		conf
	};
	if conf < 0.01 {
		conf = 0.01;
	}
	if source != USER_SOURCE && conf > MAX_AI_CONFIDENCE {
		conf = MAX_AI_CONFIDENCE;
	}
	if conf > 1.0 {
		conf = 1.0;
	}
	let kind = if conf >= FACT_CONFIDENCE {
		EntityKind::Fact
	} else {
		EntityKind::Claim
	};
	(conf, kind)
}

#[cfg(test)]
#[path = "tests/math_test.rs"]
mod math_tests;
