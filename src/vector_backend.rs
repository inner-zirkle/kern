//! The ANN index seam: a kern's vectors live either resident in an
//! [`HnswIndex`] or behind a mmap'd [`DiskIndex`] snapshot with a small
//! resident overlay — one enum so search and eviction never care which.

use std::collections::HashSet;

use super::diskann::DiskIndex;
use super::hnsw::{HnswHit, HnswIndex};
use super::util::cmp_rank;
use crate::base_types::Embedding;
use crate::quant::QuantizationMode;

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
mod tests {
	use super::*;
	use crate::diskann::{build_and_save, Params};

	fn vec_of(i: usize) -> Vec<f32> {
		(0..8)
			.map(|j| ((i as f64) * (0.13 + 0.07 * j as f64)).sin() as f32)
			.collect()
	}

	// Caller must keep the returned TempDir alive: it backs the index's mmap'd files.
	fn snapshot_over(ids: impl Iterator<Item = usize>) -> (DiskIndex, tempfile::TempDir) {
		let items: Vec<(String, Vec<f32>)> = ids.map(|i| (format!("e{i}"), vec_of(i))).collect();
		let dir = tempfile::tempdir().unwrap();
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		let idx = DiskIndex::open(dir.path()).unwrap();
		(idx, dir)
	}

	#[test]
	fn disk_backend_finds_an_insert_made_after_the_snapshot() {
		let (snap, _tmp) = snapshot_over(0..50);
		let mut be = VectorBackend::disk(snap, QuantizationMode::None);
		be.insert("e999".into(), vec_of(999).into());
		let hits = be.search(&vec_of(999), 5, 96);
		assert_eq!(
			hits.first().map(|h| h.id.as_str()),
			Some("e999"),
			"post-snapshot insert is found first"
		);
	}

	#[test]
	fn disk_backend_excludes_a_tombstoned_snapshot_id() {
		let (snap, _tmp) = snapshot_over(0..50);
		let mut be = VectorBackend::disk(snap, QuantizationMode::None);
		be.delete("e10");
		let hits = be.search(&vec_of(10), 10, 128);
		assert!(
			!hits.iter().any(|h| h.id == "e10"),
			"tombstoned id absent from results: {hits:?}"
		);
	}

	#[test]
	fn disk_union_top_hit_matches_a_single_index_over_the_whole_corpus() {
		let (snap, _tmp) = snapshot_over(0..40);
		let mut be = VectorBackend::disk(snap, QuantizationMode::None);
		for i in 40..80 {
			be.insert(format!("e{i}"), vec_of(i).into());
		}
		assert_eq!(
			be.search(&vec_of(63), 5, 128).first().map(|h| h.id.clone()),
			Some("e63".into())
		);
		assert_eq!(
			be.search(&vec_of(7), 5, 128).first().map(|h| h.id.clone()),
			Some("e7".into())
		);
	}

	#[test]
	fn disk_len_counts_live_vectors_after_delete_and_insert() {
		let (snap, _tmp) = snapshot_over(0..50);
		let mut be = VectorBackend::disk(snap, QuantizationMode::None);
		assert_eq!(be.len(), 50, "fresh snapshot len");
		be.delete("e5");
		be.insert("e500".into(), vec_of(500).into());
		assert_eq!(be.len(), 50, "49 live snapshot + 1 delta");
		assert!(!be.is_empty());
	}
}
