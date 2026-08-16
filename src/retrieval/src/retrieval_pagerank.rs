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
#[path = "tests/retrieval_pagerank_test.rs"]
mod retrieval_pagerank_tests;
