//! Tests extracted from ingest.rs
#![allow(unused)]
use super::*;

pub(crate) fn stub_one_hot(seed: &str) -> Vec<f32> {
	let h = util::content_hash(seed);
	let bytes = h.as_bytes();
	let slot = if bytes.is_empty() {
		0
	} else {
		bytes[0] as usize
	};
	let mut v = vec![0.0_f32; 256];
	v[slot] = 1.0;
	v
}
