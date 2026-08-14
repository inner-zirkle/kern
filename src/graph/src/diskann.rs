//! Vamana-style disk ANN: build a graph index into one file, mmap it, and
//! search with O(1) resident memory — the disk half of [`crate::vector_backend`].
//! Standalone build/open/search; not yet wired into the live search path.

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
	build_and_save_with_epoch(dir, items, params, None)
}

/// Same as [`build_and_save`], but skips the rebuild when the snapshot already
/// matches `epoch` (RECALL_PLAN F4). The stamp is written after the swap so a
/// crash can never leave a stamped-but-partial build; a fresh epoch file with a
/// missing/mismatched meta still falls through to a rebuild because
/// [`DiskIndex::open`] would reject it.
pub fn build_and_save_with_epoch(
	dir: &Path,
	items: &[(String, Vec<f32>)],
	params: Params,
	epoch: Option<u64>,
) -> io::Result<usize> {
	if let Some(e) = epoch {
		if snapshot_epoch(dir) == Some(e) && std::fs::metadata(meta_path(dir)).is_ok() {
			return Ok(items.len());
		}
	}
	build_and_save_into(dir, items, params, epoch)
}

/// The epoch a snapshot dir was built at; `None` when it has no stamp (or a
/// garbled one) — treated as stale by every consumer.
pub fn snapshot_epoch(dir: &Path) -> Option<u64> {
	std::fs::read_to_string(dir.join("epoch"))
		.ok()
		.and_then(|raw| raw.trim().parse().ok())
}

fn build_and_save_into(
	dir: &Path,
	items: &[(String, Vec<f32>)],
	params: Params,
	epoch: Option<u64>,
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
	if let Some(e) = epoch {
		// Best-effort: a lost stamp only costs a future rebuild, never a wrong index.
		let _ = std::fs::write(dir.join("epoch"), e.to_string());
	}
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

	// Vector for one id, if it is in this snapshot. `ids` is id-sorted by the
	// build (BTreeMap order), so binary search is exact; used by the load-time
	// reconcile to find which snapshot rows are stale (RECALL_PLAN F4).
	pub fn vector_of(&self, id: &str) -> Option<Vec<f32>> {
		let i = self.ids.binary_search_by(|x| x.as_str().cmp(id)).ok()?;
		Some(self.vec_at(i as u32))
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
#[path = "tests/diskann_test.rs"]
mod diskann_tests;
