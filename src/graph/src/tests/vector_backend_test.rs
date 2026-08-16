//! Tests extracted from vector_backend.rs
#![allow(unused)]
use super::*;

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
