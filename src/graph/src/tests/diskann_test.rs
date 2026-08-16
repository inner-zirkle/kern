//! Tests extracted from diskann.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	fn rand_items(n: usize, dim: usize, seed: u64) -> Vec<(String, Vec<f32>)> {
		use rand::RngExt;
		use rand::SeedableRng;
		let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
		(0..n)
			.map(|i| {
				let v: Vec<f32> = (0..dim).map(|_| rng.random::<f32>() - 0.5).collect();
				(format!("e{i}"), v)
			})
			.collect()
	}

	fn brute_topk(items: &[(String, Vec<f32>)], q: &[f32], k: usize) -> Vec<String> {
		let mut scored: Vec<(f32, String)> = items
			.iter()
			.map(|(id, v)| (cos_dist(q, v), id.clone()))
			.collect();
		scored.sort_by(|a, b| a.0.total_cmp(&b.0));
		scored.into_iter().take(k).map(|(_, id)| id).collect()
	}

	#[test]
	fn build_open_search_roundtrip() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(200, 16, 1);
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		let idx = DiskIndex::open(dir.path()).unwrap();
		assert_eq!(idx.len(), 200);
		let hits = idx.search(&items[0].1, 5, 64);
		assert_eq!(hits.len(), 5);
		assert_eq!(hits[0].0, "e0");
	}

	#[test]
	fn recall_at_10_is_high_vs_brute_force() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(500, 24, 7);
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		let idx = DiskIndex::open(dir.path()).unwrap();

		let queries = rand_items(20, 24, 99);
		let mut hit = 0usize;
		let mut total = 0usize;
		for (_, q) in &queries {
			let want: HashSet<String> = brute_topk(&items, q, 10).into_iter().collect();
			let got = idx.search(q, 10, 96);
			for (id, _) in got {
				if want.contains(&id) {
					hit += 1;
				}
			}
			total += want.len();
		}
		let recall = hit as f64 / total as f64;
		assert!(recall >= 0.90, "recall@10 too low: {recall:.3}");
	}

	// Sparse feature-hashed vectors, the shape `tests/e2e/test_recall.py` and the scaling
	// instruments use — and the shape that produces EXACTLY TIED cosine distances
	// in bulk. Dense random floats never tie, which is why they cannot detect a
	// tie-breaking bug.
	fn tied_items(n: usize, dim: usize) -> Vec<(String, Vec<f32>)> {
		(0..n)
			.map(|i| {
				let mut v = vec![0.0f32; dim];
				for j in 0..7 {
					let mut h: u64 = 1469598103934665603;
					for b in format!("w{}", i.wrapping_mul(2654435761).wrapping_add(j)).as_bytes() {
						h ^= *b as u64;
						h = h.wrapping_mul(1099511628211);
					}
					v[(h % dim as u64) as usize] += if h & 0x100 != 0 { 1.0 } else { -1.0 };
				}
				(format!("e{i:05}"), v)
			})
			.collect()
	}

	// A seeded RNG is not a reproducible build. Two of the three hashed containers
	// in `build_and_save` reach disk, and each was checked alone: reverting
	// `robust_prune`'s dedupe differs by 22740/76800 adjacency bytes, reverting the
	// neighbour init by 446/76800, reverting `greedy`'s visited list by none.
	// graph.bin is the whole adjacency, so comparing bytes compares the index.
	#[test]
	fn the_same_corpus_builds_a_byte_identical_index() {
		let items = tied_items(600, 64);
		let mut graphs = Vec::new();
		for _ in 0..2 {
			let dir = tempfile::tempdir().unwrap();
			build_and_save(dir.path(), &items, Params::default()).unwrap();
			graphs.push(std::fs::read(graph_path(dir.path())).unwrap());
		}
		let differing = graphs[0]
			.iter()
			.zip(&graphs[1])
			.filter(|(a, b)| a != b)
			.count();
		assert_eq!(
			differing,
			0,
			"two builds of one corpus produced different adjacency ({differing} of {} bytes differ)",
			graphs[0].len()
		);
	}

	// ROADMAP item 75: a rebuild over an existing index must swap atomically —
	// the staging dir is published in one rename, and no `.staging` dir lingers
	// to collide with the next build. Two consecutive builds over the same dir
	// both open and search correctly.
	#[test]
	fn rebuild_over_an_existing_index_swaps_and_leaves_no_staging() {
		let dir = tempfile::tempdir().unwrap();
		let a = rand_items(40, 16, 1);
		build_and_save(dir.path(), &a, Params::default()).unwrap();
		let idx_a = DiskIndex::open(dir.path()).unwrap();
		assert_eq!(idx_a.len(), 40);
		assert!(
			!dir.path().with_extension("staging").exists(),
			"no staging lingers"
		);

		// a different corpus, same shape — the swap must replace, not mix.
		let b = rand_items(40, 16, 2);
		build_and_save(dir.path(), &b, Params::default()).unwrap();
		assert!(
			!dir.path().with_extension("staging").exists(),
			"staging cleaned after second build"
		);
		let idx_b = DiskIndex::open(dir.path()).unwrap();
		assert_eq!(idx_b.len(), 40);
		// the second build's ids are the second corpus's, not a mix
		let want: std::collections::HashSet<String> = b.iter().map(|(id, _)| id.clone()).collect();
		let got: std::collections::HashSet<String> = idx_b.ids().iter().cloned().collect();
		assert_eq!(got, want, "second build is whole, not a mixed-build read");
	}

	#[test]
	fn empty_and_single() {
		let dir = tempfile::tempdir().unwrap();
		build_and_save(dir.path(), &[], Params::default()).unwrap();
		let idx = DiskIndex::open(dir.path()).unwrap();
		assert!(idx.is_empty());
		assert!(idx.search(&[1.0, 0.0], 5, 16).is_empty());

		let dir2 = tempfile::tempdir().unwrap();
		let one = vec![("solo".to_string(), vec![1.0f32, 0.0, 0.0])];
		build_and_save(dir2.path(), &one, Params::default()).unwrap();
		let idx2 = DiskIndex::open(dir2.path()).unwrap();
		let hits = idx2.search(&[1.0, 0.0, 0.0], 5, 16);
		assert_eq!(hits.len(), 1);
		assert_eq!(hits[0].0, "solo");
	}

	#[test]
	fn search_hits_filtered_returns_cosine_similarity_nearest_first() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(200, 16, 1);
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		let idx = DiskIndex::open(dir.path()).unwrap();

		let hits = idx.search_hits_filtered(&items[0].1, 5, 64, &|_| true);
		assert_eq!(hits.len(), 5);
		assert_eq!(hits[0].id, "e0", "indexed point finds itself first");
		assert!(
			hits[0].score > 0.99,
			"self-similarity ~1.0, got {}",
			hits[0].score
		);
		for w in hits.windows(2) {
			assert!(w[0].score >= w[1].score, "scores must descend: {:?}", hits);
		}
	}

	#[test]
	fn search_hits_filtered_returns_only_matching_and_is_a_subset() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(300, 16, 5);
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		let idx = DiskIndex::open(dir.path()).unwrap();

		let even = |id: &str| {
			id.trim_start_matches('e')
				.parse::<usize>()
				.map(|n| n % 2 == 0)
				.unwrap_or(false)
		};
		let q = &items[0].1;
		let filt = idx.search_hits_filtered(q, 10, 128, &even);
		assert!(!filt.is_empty(), "filtered search finds matches");
		assert!(
			filt.iter().all(|h| even(&h.id)),
			"every id passes the predicate"
		);

		let wide: HashSet<String> = idx
			.search_hits_filtered(q, 128, 128, &|_| true)
			.into_iter()
			.map(|h| h.id)
			.collect();
		assert!(
			filt.iter().all(|h| wide.contains(&h.id)),
			"filtered hits are drawn from the unfiltered candidate pool"
		);

		assert!(idx.search_hits_filtered(q, 10, 64, &|_| false).is_empty());
		assert!(idx.search_hits_filtered(q, 0, 64, &even).is_empty());
	}

	#[test]
	fn corrupt_index_is_rejected() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(10, 8, 3);
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		std::fs::write(vectors_path(dir.path()), b"short").unwrap();
		assert!(DiskIndex::open(dir.path()).is_err());
	}

	#[test]
	fn truncated_graph_is_rejected() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(10, 8, 3);
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		let full = std::fs::read(graph_path(dir.path())).unwrap();
		std::fs::write(graph_path(dir.path()), &full[..full.len() - 3]).unwrap();
		assert!(DiskIndex::open(dir.path()).is_err());
	}

	#[test]
	fn out_of_range_neighbor_is_rejected() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(10, 8, 3);
		build_and_save(dir.path(), &items, Params::default()).unwrap();
		let mut graph = std::fs::read(graph_path(dir.path())).unwrap();
		graph[..4].copy_from_slice(&(items.len() as u32 + 7).to_le_bytes());
		std::fs::write(graph_path(dir.path()), &graph).unwrap();
		assert!(DiskIndex::open(dir.path()).is_err());
	}

	fn rewrite_meta(dir: &Path, mutate: impl FnOnce(&mut Meta)) {
		let bytes = std::fs::read(meta_path(dir)).unwrap();
		let (mut meta, _): (Meta, _) =
			bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
		mutate(&mut meta);
		let out = bincode::serde::encode_to_vec(&meta, bincode::config::standard()).unwrap();
		std::fs::write(meta_path(dir), out).unwrap();
	}

	#[test]
	fn corrupt_meta_is_rejected() {
		let dir = tempfile::tempdir().unwrap();
		let items = rand_items(10, 8, 3);
		build_and_save(dir.path(), &items, Params::default()).unwrap();

		rewrite_meta(dir.path(), |m| m.entry = 999);
		assert!(
			DiskIndex::open(dir.path()).is_err(),
			"out-of-range entry point"
		);

		rewrite_meta(dir.path(), |m| {
			m.entry = 0;
			m.ids.pop();
		});
		assert!(
			DiskIndex::open(dir.path()).is_err(),
			"ids shorter than count"
		);
	}
}
