//! The ANN index seam: a kern's vectors live either resident in an
//! [`HnswIndex`] or behind a mmap'd [`DiskIndex`] snapshot with a small
//! resident overlay — one enum so search and eviction never care which.

use std::collections::HashSet;

use super::diskann::DiskIndex;
use super::hnsw::{HnswHit, HnswIndex};
use base::base_types::Embedding;
use math::quant::QuantizationMode;
use util::cmp_rank;

pub enum VectorBackend {
	Resident(HnswIndex),
	// Invariant: every delta id is tombstoned, so search (snapshot − tombstones ∪
	// delta) never serves an id twice.
	Disk {
		snapshot: DiskIndex,
		delta: HnswIndex,
		tombstones: HashSet<String>,
	},
}

impl VectorBackend {
	pub fn resident(m: usize, ef_construction: usize, quant_mode: QuantizationMode) -> Self {
		Self::Resident(HnswIndex::with_mode(m, ef_construction, quant_mode))
	}

	pub fn disk(snapshot: DiskIndex, quant_mode: QuantizationMode) -> Self {
		Self::Disk {
			snapshot,
			delta: HnswIndex::with_mode(16, 200, quant_mode),
			tombstones: HashSet::new(),
		}
	}

	// For the Disk variant this is an O(snapshot) scan — not a hot-path call.
	pub fn len(&self) -> usize {
		match self {
			Self::Resident(h) => h.len(),
			Self::Disk {
				snapshot,
				delta,
				tombstones,
			} => {
				let live_snapshot = snapshot
					.ids()
					.iter()
					.filter(|id| !tombstones.contains(*id))
					.count();
				live_snapshot + delta.len()
			}
		}
	}

	pub fn pending_delta_len(&self) -> usize {
		match self {
			Self::Resident(_) => 0,
			Self::Disk { delta, .. } => delta.len(),
		}
	}

	// A fully tombstoned but non-empty snapshot still reports non-empty.
	pub fn is_empty(&self) -> bool {
		match self {
			Self::Resident(h) => h.is_empty(),
			Self::Disk {
				snapshot, delta, ..
			} => snapshot.is_empty() && delta.is_empty(),
		}
	}

	pub fn insert(&mut self, id: String, vec: Embedding) {
		match self {
			Self::Resident(h) => h.insert(id, vec),
			Self::Disk {
				delta, tombstones, ..
			} => {
				tombstones.insert(id.clone());
				delta.insert(id, vec);
			}
		}
	}

	pub fn delete(&mut self, id: &str) {
		match self {
			Self::Resident(h) => h.delete(id),
			Self::Disk {
				delta, tombstones, ..
			} => {
				delta.delete(id);
				tombstones.insert(id.to_string());
			}
		}
	}

	pub fn search(&self, vec: &[f32], k: usize, ef: usize) -> Vec<HnswHit> {
		match self {
			Self::Resident(h) => h.search(vec, k, ef),
			Self::Disk {
				snapshot,
				delta,
				tombstones,
			} => {
				let snap = snapshot.search_hits_filtered(vec, k, ef, &|id| !tombstones.contains(id));
				let live = delta.search(vec, k, ef);
				union_rank(snap, live, k)
			}
		}
	}

	pub fn search_filtered(
		&self,
		vec: &[f32],
		k: usize,
		ef: usize,
		keep: &dyn Fn(&str) -> bool,
	) -> Vec<HnswHit> {
		match self {
			Self::Resident(h) => h.search_filtered(vec, k, ef, keep),
			Self::Disk {
				snapshot,
				delta,
				tombstones,
			} => {
				let snap =
					snapshot.search_hits_filtered(vec, k, ef, &|id| keep(id) && !tombstones.contains(id));
				let live = delta.search_filtered(vec, k, ef, keep);
				union_rank(snap, live, k)
			}
		}
	}
}

// Rank score-desc/id-asc so truncate(k) is deterministic; the higher-score dedupe
// is a defensive backstop (the Disk invariant already prevents overlap).
fn union_rank(a: Vec<HnswHit>, b: Vec<HnswHit>, k: usize) -> Vec<HnswHit> {
	use std::collections::hash_map::Entry;
	let mut by_id: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
	for h in a.into_iter().chain(b) {
		match by_id.entry(h.id) {
			Entry::Occupied(mut e) => {
				if h.score > *e.get() {
					e.insert(h.score);
				}
			}
			Entry::Vacant(e) => {
				e.insert(h.score);
			}
		}
	}
	let mut ranked: Vec<HnswHit> = by_id
		.into_iter()
		.map(|(id, score)| HnswHit { id, score })
		.collect();
	ranked.sort_by(|x, y| cmp_rank(x.score, &x.id, y.score, &y.id));
	ranked.truncate(k);
	ranked
}

#[cfg(test)]
#[path = "tests/vector_backend_test.rs"]
mod vector_backend_tests;
