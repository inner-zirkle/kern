// Not yet wired into the live search path.

use std::collections::{BTreeSet, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::hnsw::HnswHit;

// Adjacency padding marker: "no neighbour in this slot".
const SENTINEL: u32 = u32::MAX;

fn le_u32(c: &[u8]) -> u32 {
	u32::from_le_bytes([c[0], c[1], c[2], c[3]])
}

#[derive(Debug, Clone, Copy)]
pub struct Params {
	pub r: usize,
	pub build_l: usize,
	pub alpha: f32,
}

impl Default for Params {
	fn default() -> Self {
		Self {
			r: 32,
			build_l: 64,
			alpha: 1.2,
		}
	}
}

#[derive(Serialize, Deserialize)]
struct Meta {
	dim: usize,
	count: usize,
	r: usize,
	entry: u32,
	ids: Vec<String>,
}

fn meta_path(dir: &Path) -> PathBuf {
	dir.join("meta.bin")
}
fn vectors_path(dir: &Path) -> PathBuf {
	dir.join("vectors.bin")
}
fn graph_path(dir: &Path) -> PathBuf {
	dir.join("graph.bin")
}

// 1 - cos; mismatched or zero-norm inputs yield the max distance 1.0.
fn cos_dist(a: &[f32], b: &[f32]) -> f32 {
	if a.len() != b.len() {
		return 1.0;
	}
	let mut dot = 0.0f32;
	let mut na = 0.0f32;
	let mut nb = 0.0f32;
	for i in 0..a.len() {
		dot += a[i] * b[i];
		na += a[i] * a[i];
		nb += b[i] * b[i];
	}
	if na == 0.0 || nb == 0.0 {
		return 1.0;
	}
	1.0 - dot / (na.sqrt() * nb.sqrt())
}

fn greedy(
	entry: u32,
	beam_l: usize,
	dist: &mut dyn FnMut(u32) -> f32,
	neighbors: &dyn Fn(u32) -> Vec<u32>,
) -> (Vec<(f32, u32)>, Vec<u32>) {
	let mut beam: Vec<(f32, u32)> = vec![(dist(entry), entry)];
	let mut in_beam: HashSet<u32> = HashSet::from([entry]);
	// Hash order is safe HERE and only here: this list is `robust_prune`'s
	// candidate slice, and that dedupes through a BTreeSet before ranking.
	let mut visited: HashSet<u32> = HashSet::new();

	loop {
		let next = beam
			.iter()
			.filter(|(_, id)| !visited.contains(id))
			.min_by(|a, b| a.0.total_cmp(&b.0))
			.map(|&(_, id)| id);
		let Some(p) = next else { break };
		visited.insert(p);
		for nb in neighbors(p) {
			if in_beam.insert(nb) {
				beam.push((dist(nb), nb));
			}
		}
		beam.sort_by(|a, b| a.0.total_cmp(&b.0));
		if beam.len() > beam_l {
			for (_, id) in beam.drain(beam_l..) {
				in_beam.remove(&id);
			}
		}
	}
	(beam, visited.into_iter().collect())
}

fn robust_prune(
	p: u32,
	candidates: &[u32],
	r: usize,
	alpha: f32,
	vec_at: &dyn Fn(u32) -> Vec<f32>,
) -> Vec<u32> {
	let pv = vec_at(p);
	let mut scored: Vec<(f32, u32)> = candidates
		.iter()
		.copied()
		.filter(|&c| c != p)
		// BTreeSet, not HashSet: `sort_by` below is STABLE, so every TIED distance
		// keeps this order, and std's hasher is keyed per instance.
		.collect::<BTreeSet<u32>>()
		.into_iter()
		.map(|c| (cos_dist(&pv, &vec_at(c)), c))
		.collect();
	scored.sort_by(|a, b| a.0.total_cmp(&b.0));

	let mut removed = vec![false; scored.len()];
	let mut result: Vec<u32> = Vec::with_capacity(r);
	for i in 0..scored.len() {
		if removed[i] {
			continue;
		}
		if result.len() >= r {
			break;
		}
		let (_, pstar) = scored[i];
		result.push(pstar);
		let pstar_v = vec_at(pstar);
		for j in (i + 1)..scored.len() {
			if removed[j] {
				continue;
			}
			let (dpj, v) = scored[j];
			if alpha * cos_dist(&pstar_v, &vec_at(v)) <= dpj {
				removed[j] = true;
			}
		}
	}
	result
}

// Reproducible: the RNG is seeded AND every ordered container feeding the
// adjacency is ordered by construction (see `robust_prune`). The seed alone was
// not enough, and for a long time this comment claimed it was.
pub fn build_and_save(
	dir: &Path,
	items: &[(String, Vec<f32>)],
	params: Params,
) -> io::Result<usize> {
	std::fs::create_dir_all(dir)?;
	// Cross-segment atomicity (ROADMAP item 75): three independent renames used
	// to leave meta from build N+1 beside vectors from build N if a crash hit
	// between them — and the shape checks in `open` pass whenever the two builds
	// share count/dim/r, the common case. Build into a staging dir, fsync every
	// segment, then swap the staging dir over the live one in one rename. A crash
	// before the swap leaves the old build intact; a crash in the (sub-microsecond)
	// window between `remove_dir_all` and `rename` leaves no index, and `open`
	// falls back to the in-RAM index (`build_entity_disk_snapshot`), so the worst
	// case is silent staleness until the next rebuild, never a mixed-build read.
	let staging = dir.with_extension("staging");
	let _ = std::fs::remove_dir_all(&staging);
	std::fs::create_dir_all(&staging)?;
	let count = items.len();
	let dim = items.first().map(|(_, v)| v.len()).unwrap_or(0);
	let ids: Vec<String> = items.iter().map(|(id, _)| id.clone()).collect();
	let vectors: Vec<Vec<f32>> = items.iter().map(|(_, v)| v.clone()).collect();
	let vec_at = |i: u32| vectors[i as usize].clone();

	let mut adj: Vec<Vec<u32>> = vec![Vec::new(); count];
	let entry = medoid(&vectors);

	if count > 1 {
		use rand::RngExt;
		use rand::SeedableRng;
		let mut rng = rand::rngs::StdRng::seed_from_u64(42);

		for (i, slot) in adj.iter_mut().enumerate().take(count) {
			// BTreeSet, not HashSet: this seeds the traversal every later decision is
			// taken from, so hash order here reaches the built graph.
			let mut nbrs = BTreeSet::new();
			while nbrs.len() < params.r.min(count - 1) {
				let j = rng.random_range(0..count) as u32;
				if j as usize != i {
					nbrs.insert(j);
				}
			}
			*slot = nbrs.into_iter().collect();
		}

		let mut order: Vec<usize> = (0..count).collect();
		for &alpha in &[1.0f32, params.alpha] {
			for i in (1..count).rev() {
				let j = rng.random_range(0..=i);
				order.swap(i, j);
			}
			for &p in &order {
				let pv = vectors[p].clone();
				// Block scopes the borrow of `adj` so the back-edge updates can mutate it.
				let visited = {
					let mut dist = |i: u32| cos_dist(&pv, &vectors[i as usize]);
					let neighbors = |i: u32| adj[i as usize].clone();
					greedy(entry, params.build_l, &mut dist, &neighbors).1
				};
				let pruned = robust_prune(p as u32, &visited, params.r, alpha, &vec_at);
				adj[p] = pruned.clone();
				for &j in &pruned {
					let ju = j as usize;
					if !adj[ju].contains(&(p as u32)) {
						adj[ju].push(p as u32);
						if adj[ju].len() > params.r {
							let cands = adj[ju].clone();
							adj[ju] = robust_prune(j, &cands, params.r, alpha, &vec_at);
						}
					}
				}
			}
		}
	}

	write_files(&staging, dim, count, params.r, entry, &ids, &vectors, &adj)?;
	// fsync the staging dir so the new file entries are durable before the swap.
	{
		let d = std::fs::File::open(&staging)?;
		let _ = d.sync_all();
	}
	let _ = std::fs::remove_dir_all(dir);
	std::fs::rename(&staging, dir)?;
	Ok(count)
}

fn medoid(vectors: &[Vec<f32>]) -> u32 {
	if vectors.is_empty() {
		return 0;
	}
	let dim = vectors[0].len();
	let mut centroid = vec![0.0f32; dim];
	for v in vectors {
		for (c, &x) in centroid.iter_mut().zip(v.iter()) {
			*c += x;
		}
	}
	for c in &mut centroid {
		*c /= vectors.len() as f32;
	}
	let mut best = 0u32;
	let mut best_d = f32::INFINITY;
	for (i, v) in vectors.iter().enumerate() {
		let d = cos_dist(&centroid, v);
		if d < best_d {
			best_d = d;
			best = i as u32;
		}
	}
	best
}

// On-disk layout: meta.bin bincode Meta; vectors.bin count×dim f32 LE fixed
// stride; graph.bin count×r u32 LE, SENTINEL-padded.
#[allow(clippy::too_many_arguments)]
fn write_files(
	dir: &Path,
	dim: usize,
	count: usize,
	r: usize,
	entry: u32,
	ids: &[String],
	vectors: &[Vec<f32>],
	adj: &[Vec<u32>],
) -> io::Result<()> {
	let meta = Meta {
		dim,
		count,
		r,
		entry,
		ids: ids.to_vec(),
	};
	let meta_bytes = bincode::serde::encode_to_vec(&meta, bincode::config::standard())
		.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
	atomic_write(&meta_path(dir), &meta_bytes)?;

	let mut vbuf = Vec::with_capacity(count * dim * 4);
	for v in vectors {
		for &x in v {
			vbuf.extend_from_slice(&x.to_le_bytes());
		}
	}
	atomic_write(&vectors_path(dir), &vbuf)?;

	let mut gbuf = Vec::with_capacity(count * r * 4);
	for nbrs in adj {
		for slot in 0..r {
			let id = nbrs.get(slot).copied().unwrap_or(SENTINEL);
			gbuf.extend_from_slice(&id.to_le_bytes());
		}
	}
	atomic_write(&graph_path(dir), &gbuf)?;
	Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
	let tmp = path.with_extension("tmp");
	{
		let mut f = std::fs::File::create(&tmp)?;
		f.write_all(bytes)?;
		f.sync_all()?;
	}
	std::fs::rename(&tmp, path)
}

pub struct DiskIndex {
	dim: usize,
	count: usize,
	r: usize,
	entry: u32,
	ids: Vec<String>,
	vectors: Mmap,
	graph: Mmap,
}

impl DiskIndex {
	pub fn open(dir: &Path) -> io::Result<Self> {
		let corrupt = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, format!("diskann: {msg}"));
		let meta_bytes = std::fs::read(meta_path(dir))?;
		let (meta, _): (Meta, _) =
			bincode::serde::decode_from_slice(&meta_bytes, bincode::config::standard())
				.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
		if meta.ids.len() != meta.count {
			return Err(corrupt("id list length does not match meta count"));
		}
		if meta.count > 0 && meta.entry as usize >= meta.count {
			return Err(corrupt("entry point out of range"));
		}
		let vec_bytes = meta
			.count
			.checked_mul(meta.dim)
			.and_then(|n| n.checked_mul(4))
			.ok_or_else(|| corrupt("meta sizes overflow"))?;
		let graph_bytes = meta
			.count
			.checked_mul(meta.r)
			.and_then(|n| n.checked_mul(4))
			.ok_or_else(|| corrupt("meta sizes overflow"))?;
		let vectors = unsafe { Mmap::map(&std::fs::File::open(vectors_path(dir))?)? };
		let graph = unsafe { Mmap::map(&std::fs::File::open(graph_path(dir))?)? };
		// Validate sizes so a truncated/corrupt index is rejected, not read OOB.
		if vectors.len() != vec_bytes || graph.len() != graph_bytes {
			return Err(corrupt("file size does not match meta"));
		}
		// Every adjacency slot must be SENTINEL or a valid node id; otherwise the
		// beam walk would slice the vector mmap out of bounds mid-search.
		for c in graph.chunks_exact(4) {
			let id = le_u32(c);
			if id != SENTINEL && id as usize >= meta.count {
				return Err(corrupt("graph neighbor id out of range"));
			}
		}
		Ok(Self {
			dim: meta.dim,
			count: meta.count,
			r: meta.r,
			entry: meta.entry,
			ids: meta.ids,
			vectors,
			graph,
		})
	}

	pub fn len(&self) -> usize {
		self.count
	}
	pub fn is_empty(&self) -> bool {
		self.count == 0
	}

	pub fn ids(&self) -> &[String] {
		&self.ids
	}

	fn vec_at(&self, i: u32) -> Vec<f32> {
		let off = i as usize * self.dim * 4;
		self.vectors[off..off + self.dim * 4]
			.chunks_exact(4)
			.map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
			.collect()
	}

	fn neighbors_at(&self, i: u32) -> Vec<u32> {
		let off = i as usize * self.r * 4;
		self.graph[off..off + self.r * 4]
			.chunks_exact(4)
			.map(le_u32)
			.filter(|&id| id != SENTINEL)
			.collect()
	}

	pub fn search(&self, query: &[f32], k: usize, search_l: usize) -> Vec<(String, f32)> {
		if self.count == 0 || k == 0 || query.len() != self.dim {
			return Vec::new();
		}
		let beam_l = search_l.max(k);
		let mut dist = |i: u32| cos_dist(query, &self.vec_at(i));
		let neighbors = |i: u32| self.neighbors_at(i);
		let (mut beam, _) = greedy(self.entry, beam_l, &mut dist, &neighbors);
		beam.truncate(k);
		beam
			.into_iter()
			.map(|(d, i)| (self.ids[i as usize].clone(), d))
			.collect()
	}

	pub fn search_hits_filtered(
		&self,
		query: &[f32],
		k: usize,
		search_l: usize,
		keep: &dyn Fn(&str) -> bool,
	) -> Vec<HnswHit> {
		if k == 0 {
			return Vec::new();
		}
		let want = search_l.max(k);
		self
			.search(query, want, want)
			.into_iter()
			.filter(|(id, _)| keep(id))
			.take(k)
			.map(|(id, dist)| HnswHit {
				id,
				score: 1.0 - dist as f64,
			})
			.collect()
	}
}

#[cfg(test)]
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
