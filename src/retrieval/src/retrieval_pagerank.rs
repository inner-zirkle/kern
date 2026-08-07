//! Personalized PageRank over the reason graph, teleporting to the seed hits:
//! an authority signal that rewards entities the graph itself keeps pointing
//! at, fused into ranking alongside content and lexical scores.

use graph::graph::GraphGnn;
use graph::search::EntityHit;
use std::cell::Cell;
use std::collections::HashMap;

// The four vectors the walk needs are the width of the graph while a query
// touches a slice of it, so building them per call charges every query 25 bytes
// a node for pages it will not read. Lent by the thread instead of allocated,
// and handed back zeroed over the reached set alone — which is the only region
// that can hold a non-zero, by the same argument that makes the confined walk
// exact. No arithmetic and no iteration order changes, so unlike a sparse rank
// vector this does not put bit-identity in play.
#[derive(Default)]
struct Buffers {
	tele: Vec<f64>,
	rank: Vec<f64>,
	next: Vec<f64>,
	in_reached: Vec<bool>,
}

impl Buffers {
	const fn new() -> Self {
		Self {
			tele: Vec::new(),
			rank: Vec::new(),
			next: Vec::new(),
			in_reached: Vec::new(),
		}
	}

	// Grow only: a shorter graph leaves the tail allocated and zeroed, and every
	// element below the high-water mark is zero between calls.
	fn grow(&mut self, n: usize) {
		if self.tele.len() < n {
			self.tele.resize(n, 0.0);
			self.rank.resize(n, 0.0);
			self.next.resize(n, 0.0);
			self.in_reached.resize(n, false);
		}
	}
}

thread_local! {
	// Taken for the duration of a call, so a panic mid-walk leaves the thread with
	// empty vectors and the next call re-allocates rather than reading dirt.
	static BUFFERS: Cell<Buffers> = const { Cell::new(Buffers::new()) };
}

// Fills `tele` and returns its support, ascending. The support is what bounds the
// iteration: personalized mass starts there and reaches nowhere else.
fn teleport_vector(
	seeds: &[EntityHit],
	id_to_idx: &HashMap<String, usize>,
	tele: &mut [f64],
) -> Vec<usize> {
	let n = tele.len();
	let mut support: Vec<usize> = Vec::with_capacity(seeds.len());
	let mut seed_sum = 0.0;
	for s in seeds {
		if let Some(&i) = id_to_idx.get(&s.entity_id) {
			let w = s.score.max(0.0);
			support.push(i);
			tele[i] += w;
			seed_sum += w;
		}
	}
	if seed_sum > 0.0 {
		support.sort_unstable();
		support.dedup();
		// Normalising only the support leaves the rest of `tele` on its untouched
		// zero pages — the last full-width pass this function used to make.
		for &i in &support {
			tele[i] /= seed_sum;
		}
		support
	} else {
		let u = 1.0 / (n as f64);
		for t in tele.iter_mut() {
			*t = u;
		}
		(0..n).collect()
	}
}

// Both inputs ascending; `merged` is overwritten with their union, ascending.
// The sets are disjoint by construction (a node joins the reached set once).
fn merge_ascending(a: &[usize], b: &[usize], merged: &mut Vec<usize>) {
	merged.clear();
	merged.reserve(a.len() + b.len());
	let (mut i, mut j) = (0, 0);
	while i < a.len() && j < b.len() {
		if a[i] <= b[j] {
			merged.push(a[i]);
			i += 1;
		} else {
			merged.push(b[j]);
			j += 1;
		}
	}
	merged.extend_from_slice(&a[i..]);
	merged.extend_from_slice(&b[j..]);
}

// One power iteration over every node, for use when the reached set is closed and
// covers nearly all of them. Identical to the confined body below term for term:
// the nodes it adds are exactly those holding 0.0 in both vectors, so each extra
// term is a literal +0.0 and the surviving ones keep their ascending order.
fn full_width_step(
	out: &[Vec<usize>],
	tele: &[f64],
	rank: &[f64],
	next: &mut [f64],
	d: f64,
) -> f64 {
	let mut dangling = 0.0;
	for (j, outs) in out.iter().enumerate() {
		if outs.is_empty() {
			dangling += rank[j];
		}
	}
	let dangling_mass = d * dangling;
	let base = 1.0 - d + dangling_mass;
	for (slot, &t) in next.iter_mut().zip(tele.iter()) {
		*slot = base * t;
	}
	for (j, outs) in out.iter().enumerate() {
		if outs.is_empty() {
			continue;
		}
		let share = d * rank[j] / (outs.len() as f64);
		for &ti in outs {
			next[ti] += share;
		}
	}
	next
		.iter()
		.zip(rank.iter())
		.map(|(a, b)| (a - b).abs())
		.sum()
}

// Which loop body each iteration ran. Carried out of `pagerank_at` so a test can
// fail on the switch never firing as well as on it always firing — a two-path
// optimisation whose corpus only exercises one path tests nothing about the other.
#[derive(Debug, Default)]
struct Steps {
	confined: usize,
	full_width: usize,
}

// The reach share at which the confined walk's indirection costs more than the
// zeros the full-width loops re-add. Measured, not guessed: see
// `cost_against_full_width_by_reach`.
const FULL_WIDTH_REACH_PCT: usize = 90;

pub fn pagerank(
	g: &GraphGnn,
	seeds: &[EntityHit],
	damping: f64,
	iters: usize,
	top_k: usize,
) -> Vec<EntityHit> {
	pagerank_at(g, seeds, damping, iters, top_k, FULL_WIDTH_REACH_PCT).0
}

fn pagerank_at(
	g: &GraphGnn,
	seeds: &[EntityHit],
	damping: f64,
	iters: usize,
	top_k: usize,
	full_width_reach_pct: usize,
) -> (Vec<EntityHit>, Steps) {
	let adj = g.entity_adjacency();
	let ids = &adj.ids;
	let n = ids.len();
	if n == 0 {
		return (Vec::new(), Steps::default());
	}
	let out = &adj.out;
	let d = damping.clamp(0.0, 1.0);

	let mut buffers = BUFFERS.with(|b| b.take());
	buffers.grow(n);
	let Buffers {
		tele,
		rank,
		next,
		in_reached,
	} = &mut buffers;
	let tele = &mut tele[..n];
	let mut rank = &mut rank[..n];
	let mut next = &mut next[..n];
	let in_reached = &mut in_reached[..n];

	let support = teleport_vector(seeds, &adj.id_to_idx, tele);
	for &i in &support {
		rank[i] = tele[i];
	}

	// The iteration is confined to `reached` — the teleport support plus everything
	// downstream of it — because every node outside it holds an exact 0.0 in both
	// vectors, so every term the full-width loop would add for it is +0.0. Walking
	// `reached` ascending leaves the surviving terms in the full-width loop's order,
	// which is what makes this identical to it rather than merely close.
	let mut reached = support;
	for &i in &reached {
		in_reached[i] = true;
	}
	let mut fresh: Vec<usize> = Vec::new();
	let mut merged: Vec<usize> = Vec::new();
	let mut closed = reached.len() == n;
	let mut steps = Steps::default();

	// Stop early once the rank vector stops moving — `iters` is just an upper bound.
	const CONVERGENCE_EPS: f64 = 1e-9;

	for _ in 0..iters.max(1) {
		if closed && reached.len() * 100 >= n * full_width_reach_pct {
			steps.full_width += 1;
			let delta = full_width_step(out, tele, rank, next, d);
			std::mem::swap(&mut rank, &mut next);
			if delta < CONVERGENCE_EPS {
				break;
			}
			continue;
		}
		steps.confined += 1;
		let mut dangling = 0.0;
		for &j in &reached {
			if out[j].is_empty() {
				dangling += rank[j];
			}
		}
		// Dangling mass redistributed along the teleport vector (NOT uniformly) so the personalization bias is preserved.
		let dangling_mass = d * dangling;
		let base = 1.0 - d + dangling_mass;

		// Everything ever written to `next` lies in `reached`, which only grows, so
		// this also clears the values left from two iterations ago.
		for &i in &reached {
			next[i] = base * tele[i];
		}
		fresh.clear();
		for &j in &reached {
			let outs = &out[j];
			if outs.is_empty() {
				continue;
			}
			let share = d * rank[j] / (outs.len() as f64);
			for &ti in outs {
				next[ti] += share;
				if !closed && !in_reached[ti] {
					in_reached[ti] = true;
					fresh.push(ti);
				}
			}
		}
		if fresh.is_empty() {
			// An iteration that reached nothing new proves the set is closed under
			// out-edges, so it can never grow again and the per-edge membership probe
			// above — the one cost this walk adds per edge — is dead weight from here on.
			closed = true;
		} else {
			fresh.sort_unstable();
			merge_ascending(&reached, &fresh, &mut merged);
			std::mem::swap(&mut reached, &mut merged);
		}
		let delta: f64 = reached.iter().map(|&i| (next[i] - rank[i]).abs()).sum();
		std::mem::swap(&mut rank, &mut next);
		if delta < CONVERGENCE_EPS {
			break;
		}
	}

	let take = top_k.min(n);
	let mut out_list: Vec<EntityHit> = Vec::with_capacity(take);
	if take > 0 {
		// Unique ids make this a STRICT total order, so the top-k partition + sorting only the survivors equals a full sort + take.
		let cmp = |a: &(usize, f64), b: &(usize, f64)| util::cmp_rank(a.1, &ids[a.0], b.1, &ids[b.0]);
		// A zero-rank node loses to every positive one, so once the reached set alone
		// can fill top_k the untouched majority cannot enter it and never gets scanned.
		let mut scored: Vec<(usize, f64)> = reached
			.iter()
			.filter(|&&i| rank[i] > 0.0)
			.map(|&i| (i, rank[i]))
			.collect();
		if scored.len() < take {
			scored = rank.iter().copied().enumerate().collect();
		}
		if take < scored.len() {
			scored.select_nth_unstable_by(take - 1, &cmp);
			scored.truncate(take);
		}
		scored.sort_by(&cmp);
		for (idx, score) in scored {
			out_list.push(EntityHit {
				entity_id: ids[idx].clone(),
				score,
			});
		}
	}

	// Hand the buffers back as they were lent: zero everywhere. `reached` is the
	// whole of what could hold a non-zero — `tele` is written only on its support,
	// and every rank term written outside it is a literal +0.0 — so undoing it is
	// proportional to the walk and not to the graph.
	for &i in &reached {
		tele[i] = 0.0;
		rank[i] = 0.0;
		next[i] = 0.0;
		in_reached[i] = false;
	}
	BUFFERS.with(|b| b.set(buffers));
	(out_list, steps)
}

#[cfg(test)]
mod tests {
	use super::*;
	use base::base_types::Kern;

	use test_support::{edge, entity as ent};

	fn hit(id: &str, score: f64) -> EntityHit {
		EntityHit {
			entity_id: id.into(),
			score,
		}
	}

	// The full-width power iteration this file used to run, kept verbatim as the
	// reference the confined one is checked against. Only its cost is supposed to
	// have changed, so an approximation here would check nothing.
	fn pagerank_full_width(
		g: &GraphGnn,
		seeds: &[EntityHit],
		damping: f64,
		iters: usize,
		top_k: usize,
	) -> Vec<EntityHit> {
		let adj = g.entity_adjacency();
		let ids = &adj.ids;
		let n = ids.len();
		if n == 0 {
			return Vec::new();
		}
		let out = &adj.out;
		let d = damping.clamp(0.0, 1.0);
		let mut tele = vec![0.0f64; n];
		let mut seed_sum = 0.0;
		for s in seeds {
			if let Some(&i) = adj.id_to_idx.get(&s.entity_id) {
				let w = s.score.max(0.0);
				tele[i] += w;
				seed_sum += w;
			}
		}
		if seed_sum > 0.0 {
			for t in tele.iter_mut() {
				*t /= seed_sum;
			}
		} else {
			let u = 1.0 / (n as f64);
			for t in tele.iter_mut() {
				*t = u;
			}
		}

		let mut rank = tele.clone();
		let mut next = vec![0.0f64; n];
		for _ in 0..iters.max(1) {
			let mut dangling = 0.0;
			for (j, outs) in out.iter().enumerate() {
				if outs.is_empty() {
					dangling += rank[j];
				}
			}
			let base = 1.0 - d + d * dangling;
			for (i, slot) in next.iter_mut().enumerate() {
				*slot = base * tele[i];
			}
			for (j, outs) in out.iter().enumerate() {
				if outs.is_empty() {
					continue;
				}
				let share = d * rank[j] / (outs.len() as f64);
				for &ti in outs {
					next[ti] += share;
				}
			}
			let delta: f64 = next
				.iter()
				.zip(rank.iter())
				.map(|(a, b)| (a - b).abs())
				.sum();
			std::mem::swap(&mut rank, &mut next);
			if delta < 1e-9 {
				break;
			}
		}
		let take = top_k.min(n);
		if take == 0 {
			return Vec::new();
		}
		let mut scored: Vec<(usize, f64)> = rank.iter().copied().enumerate().collect();
		scored.sort_by(|a, b| util::cmp_rank(a.1, &ids[a.0], b.1, &ids[b.0]));
		scored.truncate(take);
		scored
			.into_iter()
			.map(|(idx, score)| EntityHit {
				entity_id: ids[idx].clone(),
				score,
			})
			.collect()
	}

	// Deterministic graph: `n` nodes, `fanout` out-edges each, stride-chosen so the
	// reached set is a small slice of the graph at fanout 1 and most of it at 8.
	fn synth(n: usize, fanout: usize, dangling_every: usize) -> GraphGnn {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for i in 0..n {
			k.entities
				.insert(format!("e{i:05}"), ent(&format!("e{i:05}")));
		}
		let mut h: u64 = 0x2545F491_4F6CDD1D;
		for i in 0..n {
			if i % dangling_every == 0 {
				continue;
			}
			for f in 0..fanout {
				h ^= h << 13;
				h ^= h >> 7;
				h ^= h << 17;
				let to = (h as usize) % n;
				let e = edge(&format!("e{i:05}"), &format!("e{to:05}"));
				k.reasons.insert(format!("r{i}_{f}"), e);
			}
		}
		g.register(k);
		g
	}

	// Same generator, except every edge stays inside a contiguous block of `block`
	// nodes. The seeds sit in block 0, so their reach is one block while the graph's
	// total edge count and per-node out-degree do not move with it — which is what
	// makes a sweep over `block` a sweep over reach alone.
	fn synth_blocks(n: usize, fanout: usize, dangling_every: usize, block: usize) -> GraphGnn {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for i in 0..n {
			k.entities
				.insert(format!("e{i:05}"), ent(&format!("e{i:05}")));
		}
		let mut h: u64 = 0x2545F491_4F6CDD1D;
		for i in 0..n {
			if i % dangling_every == 0 {
				continue;
			}
			let lo = (i / block) * block;
			let span = (lo + block).min(n) - lo;
			for f in 0..fanout {
				h ^= h << 13;
				h ^= h >> 7;
				h ^= h << 17;
				let to = lo + (h as usize) % span;
				let e = edge(&format!("e{i:05}"), &format!("e{to:05}"));
				k.reasons.insert(format!("r{i}_{f}"), e);
			}
		}
		g.register(k);
		g
	}

	fn reach_of(g: &GraphGnn, seeds: &[EntityHit]) -> usize {
		let adj = g.entity_adjacency();
		let mut seen = vec![false; adj.ids.len()];
		let mut stack: Vec<usize> = Vec::new();
		for s in seeds {
			if let Some(&i) = adj.id_to_idx.get(&s.entity_id) {
				if !seen[i] {
					seen[i] = true;
					stack.push(i);
				}
			}
		}
		while let Some(j) = stack.pop() {
			for &t in &adj.out[j] {
				if !seen[t] {
					seen[t] = true;
					stack.push(t);
				}
			}
		}
		seen.iter().filter(|s| **s).count()
	}

	// What a query costs in bytes and in milliseconds at each reach, on the graph
	// shape that isolates reach from edge count. The per-call buffers are flat in N,
	// so the 1%-reach row is the floor a query pays before any walking happens.
	//
	//   cargo test --release --lib -- --ignored --nocapture pagerank::tests::allocation
	#[test]
	#[ignore = "measurement, not an assertion; run explicitly in release"]
	fn allocation_and_floor_by_reach() {
		use std::time::Instant;
		use test_support::alloc_probe;
		const N: usize = 100_000;
		for pct in [1usize, 10, 50, 90, 100] {
			let block = (N * pct / 100).max(1);
			let g = synth_blocks(N, 8, 16, block);
			let seeds: Vec<EntityHit> = (0..75)
				.map(|i| hit(&format!("e{:05}", (i * (block / 75).max(1)) | 1), 1.0))
				.collect();
			let reached = reach_of(&g, &seeds);
			let _ = g.entity_adjacency();
			let run = || pagerank_at(&g, &seeds, 0.85, 25, 100, FULL_WIDTH_REACH_PCT);
			let (_, first) = alloc_probe::measure(run);
			let (_, steady) = alloc_probe::measure(run);
			let mut ms = f64::MAX;
			for _ in 0..7 {
				let t = Instant::now();
				std::hint::black_box(run());
				ms = ms.min(t.elapsed().as_secs_f64() * 1000.0);
			}
			println!(
				"N={N} block={pct:>3}% reached={reached:<7} ({:5.1}%) first={:>9}B steady={:>9}B peak={:>9}B ({:6.2} B/node) min={ms:7.3}ms",
				100.0 * reached as f64 / N as f64,
				first.total,
				steady.total,
				steady.peak,
				steady.total as f64 / N as f64,
			);
		}
	}

	// The reach sweep above cannot see the buffers, because reach and N move
	// together in it. This holds the walk fixed at ~1000 reached nodes and moves N
	// alone, which is the only shape in which a cost flat in N is legible at all.
	//
	//   cargo test --release --lib -- --ignored --nocapture pagerank::tests::floor_by
	#[test]
	#[ignore = "measurement, not an assertion; run explicitly in release"]
	fn floor_by_graph_width_at_fixed_reach() {
		use std::time::Instant;
		use test_support::alloc_probe;
		const BLOCK: usize = 1_000;
		let seeds: Vec<EntityHit> = (0..75)
			.map(|i| hit(&format!("e{:05}", (i * 13) | 1), 1.0))
			.collect();
		for n in [10_000usize, 50_000, 200_000] {
			let g = synth_blocks(n, 4, 16, BLOCK);
			let reached = reach_of(&g, &seeds);
			let _ = g.entity_adjacency();
			let run = || pagerank_at(&g, &seeds, 0.85, 25, 100, FULL_WIDTH_REACH_PCT);
			std::hint::black_box(run());
			let (_, a) = alloc_probe::measure(run);
			let mut ms = f64::MAX;
			for _ in 0..25 {
				let t = Instant::now();
				std::hint::black_box(run());
				ms = ms.min(t.elapsed().as_secs_f64() * 1000.0);
			}
			println!(
				"N={n:<8} reached={reached:<6} steady={:>9}B peak={:>9}B min={ms:7.3}ms",
				a.total, a.peak
			);
		}
	}

	#[test]
	#[ignore = "measurement, not an assertion; run explicitly in release"]
	fn cost_against_full_width_by_reach() {
		use std::time::Instant;
		const N: usize = 100_000;
		const REPS: usize = 7;
		for fanout in [1usize, 2, 4, 8, 16] {
			for pct in [60usize, 80, 90, 95, 100] {
				let block = N * pct / 100;
				let g = synth_blocks(N, fanout, 16, block);
				// `| 1` keeps every seed off the dangling stride, so the reach the row
				// reports is the block and not the seed set.
				let seeds: Vec<EntityHit> = (0..75)
					.map(|i| hit(&format!("e{:05}", (i * (block / 75)) | 1), 1.0))
					.collect();
				let reached = reach_of(&g, &seeds);
				let _ = g.entity_adjacency();
				let mut confined = f64::MAX;
				let mut shipped = f64::MAX;
				let mut steps = Steps::default();
				for _ in 0..REPS {
					// Both sides are the same function with the same top-k tail; only the
					// loop body differs, which is the only thing this row is deciding. 101
					// is the confined-only walk this switch was measured against.
					let t = Instant::now();
					let a = pagerank_at(&g, &seeds, 0.85, 25, 100, 101);
					confined = confined.min(t.elapsed().as_secs_f64() * 1000.0);
					let t = Instant::now();
					let b = pagerank_at(&g, &seeds, 0.85, 25, 100, FULL_WIDTH_REACH_PCT);
					shipped = shipped.min(t.elapsed().as_secs_f64() * 1000.0);
					assert_eq!(a.0.len(), b.0.len());
					assert_eq!(a.1.full_width, 0);
					steps = b.1;
				}
				println!(
					"N={N} fanout={fanout:<3} block={pct:>3}% reached={reached:<7} ({:5.1}%) confined_only={confined:7.3}ms shipped={shipped:7.3}ms ratio={:5.2} steps={steps:?}",
					100.0 * reached as f64 / N as f64,
					shipped / confined
				);
			}
		}
	}

	// The confined iteration's win is bounded by how far the seeds reach, so the
	// number it earns is a property of the graph, not of the code. Kept as the
	// instrument for that claim.
	//
	//   cargo test --release --lib -- --ignored --nocapture pagerank::tests::cost
	#[test]
	#[ignore = "measurement, not an assertion; run explicitly in release"]
	fn cost_against_full_width_by_fanout() {
		use std::time::Instant;
		const N: usize = 100_000;
		let seeds: Vec<EntityHit> = (0..75)
			.map(|i| hit(&format!("e{:05}", i * 137), 1.0))
			.collect();
		for fanout in [1usize, 4, 16] {
			let g = synth(N, fanout, 2);
			// The adjacency is built once per graph and cached on it, so whichever call
			// ran first would otherwise be charged the whole build.
			let _ = g.entity_adjacency();
			let t = Instant::now();
			let a = pagerank_at(&g, &seeds, 0.85, 25, 100, FULL_WIDTH_REACH_PCT);
			let shipped = t.elapsed().as_secs_f64() * 1000.0;
			let t = Instant::now();
			let b = pagerank_full_width(&g, &seeds, 0.85, 25, 100);
			let full = t.elapsed().as_secs_f64() * 1000.0;
			assert_eq!(a.0.len(), b.len());
			println!(
				"N={N} fanout={fanout:<3} shipped={shipped:7.3}ms full_width={full:7.3}ms steps={:?}",
				a.1
			);
		}
	}

	// The one thing that can witness the buffers is the allocator. The ranking is
	// identical whether they are lent or built — that is the point of the change —
	// and the clock on a shared box cannot resolve 2 MB of `calloc`.
	//
	// Both graphs are generated with the same seed stream and the same block size,
	// so block 0 holds byte-identical edges in both and the query walks the same
	// nodes in the same order. Every allocation the walk makes is therefore a
	// function of the reached set, which is equal — except for anything sized by
	// the graph, which is 4x larger on the right and must not exist.
	#[test]
	fn a_narrow_query_allocates_nothing_sized_by_the_graph() {
		use test_support::alloc_probe;
		const BLOCK: usize = 250;
		let small = synth_blocks(4_000, 4, 16, BLOCK);
		let big = synth_blocks(16_000, 4, 16, BLOCK);
		let seeds: Vec<EntityHit> = (0..20)
			.map(|i| hit(&format!("e{:05}", (i * 11) | 1), 1.0))
			.collect();
		let (rs, rb) = (reach_of(&small, &seeds), reach_of(&big, &seeds));
		assert_eq!(rs, rb, "the two graphs must present the same walk");
		assert!(
			(50..=BLOCK).contains(&rs),
			"the query must stay inside its block and still fill top-k: reached {rs}"
		);

		let run = |g: &GraphGnn| pagerank_at(g, &seeds, 0.85, 25, 50, FULL_WIDTH_REACH_PCT);
		// Warm both first: the adjacency is built on demand, and a pool grown to the
		// larger graph would otherwise charge that one growth to the measurement.
		std::hint::black_box((run(&small), run(&big)));
		let (_, a) = alloc_probe::measure(|| run(&small));
		let (_, b) = alloc_probe::measure(|| run(&big));
		assert_eq!(
			b.total, a.total,
			"a 4x larger graph cost {} B against {} B for the same {rs}-node walk",
			b.total, a.total
		);
		assert_eq!(
			b.peak, a.peak,
			"largest single block was {} B on the 4x larger graph against {} B on the small one",
			b.peak, a.peak
		);
	}

	#[test]
	fn confined_iteration_equals_the_full_width_one_bit_for_bit() {
		let mut cases = 0;
		let mut ran = Steps::default();
		// The last two straddle the near-N switch: at n=600, fanout 6, edges confined
		// to a block, the reach is a known fraction either side of FULL_WIDTH_REACH_PCT.
		let graphs = [
			synth(400, 1, 3),
			synth(400, 8, 7),
			synth(60, 3, 2),
			synth_blocks(600, 6, 9, 600 * 88 / 100),
			synth_blocks(600, 6, 9, 600 * 92 / 100),
		];
		let straddle = hit("e00003", 1.0);
		for (gi, want_below) in [(3usize, true), (4, false)] {
			let n = graphs[gi].entity_adjacency().ids.len();
			let r = reach_of(&graphs[gi], std::slice::from_ref(&straddle));
			assert_eq!(
				r * 100 < n * FULL_WIDTH_REACH_PCT,
				want_below,
				"graph {gi} must sit on its side of the switch, reached {r} of {n}"
			);
		}
		for (gi, g) in graphs.iter().enumerate() {
			let n = g.entity_adjacency().ids.len();
			for seed_ids in [
				vec![],
				vec![("e00007", 1.0)],
				vec![("e00003", 0.9), ("e00011", 0.2), ("e00019", 0.4)],
				// A zero-weight seed contributes no teleport mass but still names a node.
				vec![("e00003", 0.0), ("e00003", 0.5), ("e00011", -1.0)],
			] {
				let seeds: Vec<EntityHit> = seed_ids.iter().map(|(i, s)| hit(i, *s)).collect();
				for top_k in [3usize, 100, n * 2] {
					for iters in [1usize, 4, 25] {
						// 101 never switches, 0 switches the moment the set closes, and
						// FULL_WIDTH_REACH_PCT is what ships. All three answer to full width.
						for pct in [101usize, FULL_WIDTH_REACH_PCT, 0] {
							let (got, steps) = pagerank_at(g, &seeds, 0.85, iters, top_k, pct);
							ran.confined += steps.confined;
							ran.full_width += steps.full_width;
							let want = pagerank_full_width(g, &seeds, 0.85, iters, top_k);
							assert_eq!(
								got.len(),
								want.len(),
								"g={gi} pct={pct} top_k={top_k} iters={iters} length"
							);
							for (a, b) in got.iter().zip(want.iter()) {
								assert_eq!(
									a.entity_id, b.entity_id,
									"g={gi} pct={pct} top_k={top_k} iters={iters} order"
								);
								assert_eq!(
									a.score.to_bits(),
									b.score.to_bits(),
									"g={gi} pct={pct} top_k={top_k} iters={iters} score for {}: {} vs {}",
									a.entity_id,
									a.score,
									b.score
								);
							}
							cases += 1;
						}
					}
				}
			}
		}
		assert_eq!(cases, 540, "every configuration was actually compared");
		// A matrix that only ever walked one of the two bodies would prove nothing
		// about the other, and would still pass every assertion above.
		assert!(
			ran.confined > 0 && ran.full_width > 0,
			"both bodies ran: {ran:?}"
		);
	}

	// The switch is only worth its second loop body if it fires where the measurement
	// says it should and nowhere else, so both directions are asserted: a graph the
	// seeds saturate must take it, and one they barely touch must not.
	#[test]
	fn the_near_n_body_runs_on_saturating_reach_and_not_on_narrow_reach() {
		const N: usize = 4_000;
		let seeds: Vec<EntityHit> = (0..20)
			.map(|i| hit(&format!("e{:05}", (i * 13) | 1), 1.0))
			.collect();

		let wide = synth_blocks(N, 8, 16, N);
		let wide_reach = reach_of(&wide, &seeds);
		assert!(
			wide_reach * 100 >= N * FULL_WIDTH_REACH_PCT,
			"the saturating graph must actually saturate, reached {wide_reach} of {N}"
		);
		let (_, wide_steps) = pagerank_at(&wide, &seeds, 0.85, 25, 100, FULL_WIDTH_REACH_PCT);
		assert!(
			wide_steps.full_width > 0,
			"a reach of {wide_reach}/{N} must run the near-N body: {wide_steps:?}"
		);

		let narrow = synth_blocks(N, 8, 16, N / 10);
		let narrow_reach = reach_of(&narrow, &seeds);
		assert!(
			narrow_reach * 100 < N * FULL_WIDTH_REACH_PCT,
			"the narrow graph must stay narrow, reached {narrow_reach} of {N}"
		);
		let (_, narrow_steps) = pagerank_at(&narrow, &seeds, 0.85, 25, 100, FULL_WIDTH_REACH_PCT);
		assert_eq!(
			narrow_steps.full_width, 0,
			"a reach of {narrow_reach}/{N} must never run the near-N body: {narrow_steps:?}"
		);
		assert!(narrow_steps.confined > 0, "{narrow_steps:?}");
	}

	#[test]
	fn rank_reaches_past_the_seed_and_its_first_hop() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for id in ["A", "B", "C", "D", "Z"] {
			k.entities.insert(id.into(), ent(id));
		}
		for e in [edge("A", "B"), edge("B", "C"), edge("C", "D")] {
			k.reasons.insert(e.id.clone(), e);
		}
		g.register(k);

		let r = pagerank(&g, &[hit("A", 1.0)], 0.85, 25, 5);
		let s = |id: &str| r.iter().find(|h| h.entity_id == id).unwrap().score;
		for id in ["B", "C", "D"] {
			assert!(
				s(id) > 0.0,
				"{id} is downstream of the seed and must be ranked"
			);
		}
		assert!(
			s("B") > s("C") && s("C") > s("D"),
			"rank decays with distance"
		);
		assert_eq!(s("Z"), 0.0, "Z is unreachable from the seed");
	}

	#[test]
	fn a_new_edge_changes_the_ranking_it_should_change() {
		let build = |extra: bool| {
			let mut g = GraphGnn::new();
			let mut k = Kern::new("k", "");
			for id in ["A", "B", "C"] {
				k.entities.insert(id.into(), ent(id));
			}
			k.reasons.insert("A->B".into(), edge("A", "B"));
			g.register(k);
			let before = pagerank(&g, &[hit("A", 1.0)], 0.85, 25, 3);
			if !extra {
				return before;
			}
			g.get_mut("k")
				.unwrap()
				.reasons
				.insert("A->C".into(), edge("A", "C"));
			pagerank(&g, &[hit("A", 1.0)], 0.85, 25, 3)
		};
		let s = |v: &[EntityHit], id: &str| v.iter().find(|h| h.entity_id == id).unwrap().score;
		let before = build(false);
		let after = build(true);
		assert_eq!(
			s(&before, "C"),
			0.0,
			"C is unreached before the edge exists"
		);
		assert!(
			s(&after, "C") > 0.0,
			"the edge added after the first query must be visible to the second"
		);
		assert!(
			s(&after, "B") < s(&before, "B"),
			"B now splits A's outflow with C ({} vs {})",
			s(&after, "B"),
			s(&before, "B")
		);
	}

	#[test]
	fn empty_graph_is_empty() {
		assert!(pagerank(&GraphGnn::new(), &[], 0.85, 10, 5).is_empty());
	}

	#[test]
	fn ranks_hub_above_leaves_and_sums_to_one() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for id in ["A", "B", "C"] {
			k.entities.insert(id.into(), ent(id));
		}
		for e in [edge("B", "A"), edge("C", "A")] {
			k.reasons.insert(e.id.clone(), e);
		}
		g.register(k);

		let ranks = pagerank(&g, &[], 0.85, 100, 3);
		assert_eq!(ranks.len(), 3);
		let score = |id: &str| ranks.iter().find(|h| h.entity_id == id).unwrap().score;
		assert!(score("A") > score("B"), "hub A must outrank leaf B");
		let sum: f64 = ranks.iter().map(|h| h.score).sum();
		assert!((sum - 1.0).abs() < 1e-6, "ranks sum ~1, got {sum}");
	}

	#[test]
	fn self_loops_do_not_inflate_score() {
		let make = |with_loop: bool| {
			let mut g = GraphGnn::new();
			let mut k = Kern::new("k", "");
			for id in ["A", "B"] {
				k.entities.insert(id.into(), ent(id));
			}
			k.reasons.insert("B->A".into(), edge("B", "A"));
			if with_loop {
				k.reasons.insert("A->A".into(), edge("A", "A"));
			}
			g.register(k);
			pagerank(&g, &[], 0.85, 100, 2)
		};
		let s = |v: &[EntityHit], id: &str| v.iter().find(|h| h.entity_id == id).unwrap().score;
		let base = make(false);
		let looped = make(true);
		assert!(
			(s(&base, "A") - s(&looped, "A")).abs() < 1e-9,
			"self-loop must not change A's rank ({} vs {})",
			s(&base, "A"),
			s(&looped, "A")
		);
	}

	#[test]
	fn convergence_early_exit_matches_full_iteration() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for id in ["A", "B", "C"] {
			k.entities.insert(id.into(), ent(id));
		}
		for e in [edge("A", "B"), edge("B", "C"), edge("C", "A")] {
			k.reasons.insert(e.id.clone(), e);
		}
		g.register(k);
		let few = pagerank(&g, &[], 0.85, 5, 3);
		let many = pagerank(&g, &[], 0.85, 1000, 3);
		for (a, b) in few.iter().zip(many.iter()) {
			assert_eq!(a.entity_id, b.entity_id);
			assert!(
				(a.score - b.score).abs() < 1e-6,
				"converged result is iteration-count-independent"
			);
		}
	}

	#[test]
	fn top_k_partition_matches_full_sort_prefix() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		let nodes = ["A", "B", "C", "D", "E", "F", "G", "H"];
		for id in nodes {
			k.entities.insert(id.into(), ent(id));
		}
		for e in [
			edge("A", "D"),
			edge("B", "D"),
			edge("C", "D"),
			edge("D", "E"),
			edge("F", "E"),
			edge("G", "E"),
			edge("H", "A"),
		] {
			k.reasons.insert(e.id.clone(), e);
		}
		g.register(k);
		let full = pagerank(&g, &[], 0.85, 200, nodes.len());
		let topk = pagerank(&g, &[], 0.85, 200, 3);
		assert_eq!(topk.len(), 3, "top_k truncates to 3");
		for i in 0..3 {
			assert_eq!(
				topk[i].entity_id, full[i].entity_id,
				"top-{i} id matches full prefix"
			);
			assert!(
				(topk[i].score - full[i].score).abs() < 1e-12,
				"top-{i} score matches"
			);
		}
	}

	#[test]
	fn ties_break_by_id_ascending_under_top_k() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for id in ["C", "A", "B"] {
			k.entities.insert(id.into(), ent(id));
		}
		g.register(k);
		let r = pagerank(&g, &[], 0.85, 50, 1);
		assert_eq!(r.len(), 1);
		assert_eq!(
			r[0].entity_id, "A",
			"tied ranks resolve to the id-ascending winner"
		);
	}

	#[test]
	fn personalization_biases_toward_seed_and_conserves_mass() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for id in ["A", "B", "X", "Y"] {
			k.entities.insert(id.into(), ent(id));
		}
		for e in [edge("B", "A"), edge("Y", "X")] {
			k.reasons.insert(e.id.clone(), e);
		}
		g.register(k);

		let global = pagerank(&g, &[], 0.85, 200, 4);
		let gscore = |id: &str| global.iter().find(|h| h.entity_id == id).unwrap().score;
		assert!((gscore("A") - gscore("X")).abs() < 1e-6, "A,X symmetric");

		let seeded = pagerank(&g, &[hit("Y", 1.0)], 0.85, 200, 4);
		let sscore = |id: &str| seeded.iter().find(|h| h.entity_id == id).unwrap().score;
		assert!(sscore("X") > gscore("X"), "seeded X must beat global X");
		assert!(sscore("X") > sscore("A"), "seeded X-component outranks A");
		let sum: f64 = seeded.iter().map(|h| h.score).sum();
		assert!((sum - 1.0).abs() < 1e-6, "seeded ranks sum ~1, got {sum}");
	}
}
