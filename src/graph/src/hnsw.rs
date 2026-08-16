//! In-memory HNSW index over cosine distance, storing vectors in the store's
//! quantization mode so RAM cost matches disk cost. Deterministic level
//! assignment (hash of the id, not RNG) keeps rebuilds reproducible.

use math::cosine_distance;
use math::quant::{quantized_cosine_distance, QuantizationMode, QuantizedVec};
use std::collections::HashMap;
use std::marker::PhantomData;
use util::{cmp_partial, content_hash};

#[derive(Debug, Clone)]
pub struct HnswHit {
	pub id: String,
	pub score: f64,
}

struct HnswNode {
	vec: base::base_types::Embedding,
	qvec: Option<QuantizedVec>,
	layers: Vec<Vec<u32>>,
}

#[derive(Clone, Copy)]
struct Candidate {
	id: u32,
	dist: f64,
}

pub struct HnswIndex {
	m: usize,
	m0: usize,
	ef_construction: usize,
	ml: f64,
	nodes: Vec<Option<HnswNode>>,
	id_of: Vec<String>,
	slot_of: HashMap<String, u32>,
	free: Vec<u32>,
	// Deleted, not yet scrubbed of inbound edges, and therefore not yet reusable.
	pending_scrub: Vec<u32>,
	ep: Option<u32>,
	max_layer: usize,
	quant_mode: QuantizationMode,
}

enum Query<'a> {
	Float(&'a [f32]),
	Int8 { q: QuantizedVec, raw: &'a [f32] },
	Binary { q: QuantizedVec, raw: &'a [f32] },
}

impl<'a> Query<'a> {
	fn new(vec: &'a [f32], mode: QuantizationMode) -> Self {
		match mode {
			QuantizationMode::Int8 => Self::Int8 {
				q: QuantizedVec::encode(vec, QuantizationMode::Int8),
				raw: vec,
			},
			QuantizationMode::Binary => Self::Binary {
				q: QuantizedVec::encode(vec, QuantizationMode::Binary),
				raw: vec,
			},
			_ => Self::Float(vec),
		}
	}
}

impl HnswIndex {
	pub fn new(m: usize, ef_construction: usize) -> Self {
		Self::with_mode(m, ef_construction, QuantizationMode::None)
	}

	pub fn with_mode(m: usize, ef_construction: usize, quant_mode: QuantizationMode) -> Self {
		let m = m.max(2);
		Self {
			m,
			m0: m * 2,
			ef_construction,
			ml: 1.0 / (m as f64).ln(),
			nodes: Vec::new(),
			id_of: Vec::new(),
			slot_of: HashMap::new(),
			free: Vec::new(),
			pending_scrub: Vec::new(),
			ep: None,
			max_layer: 0,
			quant_mode,
		}
	}

	// A deleted slot is live in neither list: `free` holds only scrubbed slots, and
	// `pending_scrub` holds deleted ones awaiting their pass. Counting `free` alone
	// would report a deleted node as present until the next insert.
	pub fn len(&self) -> usize {
		self.nodes.len() - self.free.len() - self.pending_scrub.len()
	}

	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	fn node(&self, slot: u32) -> Option<&HnswNode> {
		self.nodes.get(slot as usize).and_then(|n| n.as_ref())
	}

	fn node_mut(&mut self, slot: u32) -> Option<&mut HnswNode> {
		self.nodes.get_mut(slot as usize).and_then(|n| n.as_mut())
	}

	fn id_str(&self, slot: u32) -> &str {
		&self.id_of[slot as usize]
	}

	fn alloc_slot(&mut self, id: String, node: HnswNode) -> u32 {
		// A slot may only be reused once its inbound edges are gone, or a lingering
		// edge silently points at whatever lands in it next.
		self.scrub_pending();
		if let Some(slot) = self.free.pop() {
			self.nodes[slot as usize] = Some(node);
			self.id_of[slot as usize] = id.clone();
			self.slot_of.insert(id, slot);
			slot
		} else {
			let slot = self.nodes.len() as u32;
			self.nodes.push(Some(node));
			self.id_of.push(id.clone());
			self.slot_of.insert(id, slot);
			slot
		}
	}

	// Scrubbing inbound edges costs a pass over every node and every layer, and a
	// GC sweep deletes many entities at once — doing it per delete made a sweep
	// O(victims x nodes x edges). Deletion is therefore two steps: mark the node
	// dead now (searches skip a `None` node, so it is immediately invisible), and
	// scrub every pending slot in ONE pass before any of them can be reused.
	//
	// Symmetry is not enough to do better: insert links both ways, but pruning a
	// neighbour's over-cap list drops its back-edge while the forward edge remains,
	// so a node's own layers are not a complete list of who points at it.
	pub fn delete(&mut self, id: &str) {
		let Some(slot) = self.slot_of.remove(id) else {
			return;
		};
		self.nodes[slot as usize] = None;
		self.pending_scrub.push(slot);
		if self.ep == Some(slot) {
			self.ep = self
				.nodes
				.iter()
				.position(|n| n.is_some())
				.map(|i| i as u32);
		}
	}

	// One pass for every slot deleted since the last one. Only after this may a
	// slot enter `free` — until then nothing can alias it.
	fn scrub_pending(&mut self) {
		if self.pending_scrub.is_empty() {
			return;
		}
		let dead: std::collections::HashSet<u32> = self.pending_scrub.iter().copied().collect();
		for n in self.nodes.iter_mut().flatten() {
			for layer in n.layers.iter_mut() {
				layer.retain(|s| !dead.contains(s));
			}
		}
		self.free.append(&mut self.pending_scrub);
	}

	pub fn insert(&mut self, id: String, vec: base::base_types::Embedding) {
		if vec.is_empty() || self.slot_of.contains_key(&id) {
			return;
		}
		let level = self.level_for(&id);
		let (stored_vec, qvec) = match self.quant_mode {
			QuantizationMode::Int8 | QuantizationMode::Binary => (
				base::base_types::Embedding::from(&[][..]),
				Some(QuantizedVec::encode(&vec, self.quant_mode)),
			),
			_ => (vec.clone(), None),
		};
		let node = HnswNode {
			vec: stored_vec,
			qvec,
			layers: vec![Vec::new(); level + 1],
		};
		let slot = self.alloc_slot(id, node);

		let Some(mut ep) = self.ep else {
			self.ep = Some(slot);
			self.max_layer = level;
			return;
		};

		let query = Query::new(&vec, self.quant_mode);

		for l in (level + 1..=self.max_layer).rev() {
			ep = self.greedy_nearest(ep, &query, l);
		}

		let start = level.min(self.max_layer);
		for l in (0..=start).rev() {
			let cap = if l == 0 { self.m0 } else { self.m };
			let candidates = self.beam_search(ep, &query, l, self.ef_construction);
			let neighbors: Vec<Candidate> = candidates.iter().take(cap).copied().collect();

			{
				let node = self.node_mut(slot).expect("node just inserted above");
				while node.layers.len() <= l {
					node.layers.push(Vec::new());
				}
				node.layers[l] = neighbors.iter().map(|n| n.id).collect();
			}

			for nb in &neighbors {
				let over_cap = {
					let nb_node = match self.node_mut(nb.id) {
						Some(n) => n,
						None => continue,
					};
					while nb_node.layers.len() <= l {
						nb_node.layers.push(Vec::new());
					}
					nb_node.layers[l].push(slot);
					nb_node.layers[l].len() > cap
				};
				if over_cap {
					let ids: Vec<u32> = self
						.node(nb.id)
						.expect("nb_node fetched via node_mut earlier in loop")
						.layers[l]
						.clone();
					let pruned = self.prune_neighbors(nb.id, &ids, cap);
					self
						.node_mut(nb.id)
						.expect("nb_node fetched via node_mut earlier in loop")
						.layers[l] = pruned;
				}
			}

			if let Some(c) = candidates.first() {
				ep = c.id;
			}
		}

		if level > self.max_layer {
			self.max_layer = level;
			self.ep = Some(slot);
		}
	}

	pub fn search(&self, vec: &[f32], k: usize, ef: usize) -> Vec<HnswHit> {
		let Some(mut ep) = self.ep else {
			return Vec::new();
		};
		if vec.is_empty() {
			return Vec::new();
		}
		let query = Query::new(vec, self.quant_mode);
		let ef = ef.max(k);

		for l in (1..=self.max_layer).rev() {
			ep = self.greedy_nearest(ep, &query, l);
		}

		let candidates = self.beam_search(ep, &query, 0, ef);
		let k = k.min(candidates.len());
		candidates[..k]
			.iter()
			.map(|c| HnswHit {
				id: self.id_str(c.id).to_string(),
				score: 1.0 - c.dist,
			})
			.collect()
	}

	pub fn search_filtered(
		&self,
		vec: &[f32],
		k: usize,
		ef: usize,
		keep: &dyn Fn(&str) -> bool,
	) -> Vec<HnswHit> {
		let Some(mut ep) = self.ep else {
			return Vec::new();
		};
		if vec.is_empty() || k == 0 {
			return Vec::new();
		}
		let query = Query::new(vec, self.quant_mode);
		let ef = ef.max(k);
		// Upper layers are unfiltered navigation; only the layer-0 beam filters.
		for l in (1..=self.max_layer).rev() {
			ep = self.greedy_nearest(ep, &query, l);
		}
		let candidates = self.beam_search_filtered(ep, &query, 0, ef, keep);
		let k = k.min(candidates.len());
		candidates[..k]
			.iter()
			.map(|c| HnswHit {
				id: self.id_str(c.id).to_string(),
				score: 1.0 - c.dist,
			})
			.collect()
	}

	// Frontier admits every visited node; results admit only keep matches, so
	// matches behind walls of non-matching nodes are still found.
	fn beam_search_filtered(
		&self,
		ep: u32,
		query: &Query<'_>,
		layer: usize,
		ef: usize,
		keep: &dyn Fn(&str) -> bool,
	) -> Vec<Candidate> {
		let ep_dist = self.distance_to_query(ep, query);
		let mut candidates = MinHeap::new();
		let mut results = MaxHeap::new();
		let mut visited = vec![false; self.nodes.len()];

		let seed = Candidate {
			id: ep,
			dist: ep_dist,
		};
		candidates.push(seed);
		visited[ep as usize] = true;
		if keep(self.id_str(ep)) {
			results.push(seed);
		}

		while let Some(c) = candidates.pop() {
			if results.len() >= ef {
				if let Some(worst) = results.peek() {
					if c.dist > worst.dist {
						break;
					}
				}
			}
			let node = match self.node(c.id) {
				Some(n) => n,
				None => continue,
			};
			if layer >= node.layers.len() {
				continue;
			}
			for &nb in &node.layers[layer] {
				let vi = nb as usize;
				if vi >= visited.len() || visited[vi] {
					continue;
				}
				visited[vi] = true;
				if self.node(nb).is_none() {
					continue;
				}
				let d = self.distance_to_query(nb, query);
				let worst = results.peek().map(|w| w.dist);
				let explore = results.len() < ef || worst.is_none_or(|w| d < w);
				if explore {
					candidates.push(Candidate { id: nb, dist: d });
					if keep(self.id_str(nb)) {
						results.push(Candidate { id: nb, dist: d });
						if results.len() > ef {
							results.pop();
						}
					}
				}
			}
		}

		let mut out = Vec::with_capacity(results.len());
		while let Some(c) = results.pop() {
			out.push(c);
		}
		out.reverse();
		out
	}

	// Id-stable layer: the same id lands on the same level under any insert order or churn.
	fn level_for(&self, id: &str) -> usize {
		let mut h: u64 = 0xcbf2_9ce4_8422_2325;
		for &b in id.as_bytes() {
			h ^= b as u64;
			h = h.wrapping_mul(0x100_0000_01b3);
		}
		let r = ((h >> 11) as f64 / (1u64 << 53) as f64).max(1e-18);
		let level = (-r.ln() * self.ml).floor() as usize;
		level.min(16)
	}

	fn distance_to_query(&self, slot: u32, query: &Query<'_>) -> f64 {
		let node = match self.node(slot) {
			Some(n) => n,
			None => return 1.0,
		};
		match query {
			Query::Float(v) => cosine_distance(&node.vec, v),
			Query::Int8 { q, raw } | Query::Binary { q, raw } => match &node.qvec {
				Some(nq) => quantized_cosine_distance(nq, q),
				None => cosine_distance(&node.vec, raw),
			},
		}
	}

	fn distance_between(&self, a: u32, b: u32) -> f64 {
		let (Some(na), Some(nb)) = (self.node(a), self.node(b)) else {
			return 1.0;
		};
		match self.quant_mode {
			QuantizationMode::Int8 | QuantizationMode::Binary => match (&na.qvec, &nb.qvec) {
				(Some(qa), Some(qb)) => quantized_cosine_distance(qa, qb),
				_ => cosine_distance(&na.vec, &nb.vec),
			},
			_ => cosine_distance(&na.vec, &nb.vec),
		}
	}

	fn greedy_nearest(&self, ep: u32, query: &Query<'_>, layer: usize) -> u32 {
		let mut best = ep;
		let mut best_dist = self.distance_to_query(ep, query);
		loop {
			let mut changed = false;
			let node = match self.node(best) {
				Some(n) => n,
				None => break,
			};
			if layer >= node.layers.len() {
				break;
			}
			for &nb in &node.layers[layer] {
				if self.node(nb).is_some() {
					let d = self.distance_to_query(nb, query);
					if d < best_dist {
						best_dist = d;
						best = nb;
						changed = true;
					}
				}
			}
			if !changed {
				break;
			}
		}
		best
	}

	fn beam_search(&self, ep: u32, query: &Query<'_>, layer: usize, ef: usize) -> Vec<Candidate> {
		let ep_dist = self.distance_to_query(ep, query);
		let mut candidates = MinHeap::new();
		let mut results = MaxHeap::new();
		let mut visited = vec![false; self.nodes.len()];

		let seed = Candidate {
			id: ep,
			dist: ep_dist,
		};
		candidates.push(seed);
		results.push(seed);
		visited[ep as usize] = true;

		while let Some(c) = candidates.pop() {
			if results.len() >= ef {
				if let Some(worst) = results.peek() {
					if c.dist > worst.dist {
						break;
					}
				}
			}
			let node = match self.node(c.id) {
				Some(n) => n,
				None => continue,
			};
			if layer >= node.layers.len() {
				continue;
			}
			for &nb in &node.layers[layer] {
				let vi = nb as usize;
				if vi >= visited.len() || visited[vi] {
					continue;
				}
				visited[vi] = true;
				if self.node(nb).is_none() {
					continue;
				}
				let d = self.distance_to_query(nb, query);
				let dominated = results.len() >= ef && results.peek().is_some_and(|w| d >= w.dist);
				if !dominated {
					let cand = Candidate { id: nb, dist: d };
					candidates.push(cand);
					results.push(cand);
					if results.len() > ef {
						results.pop();
					}
				}
			}
		}

		let mut out = Vec::with_capacity(results.len());
		while let Some(c) = results.pop() {
			out.push(c);
		}
		out.reverse();
		out
	}

	// Slot-independent canonical digest; determinism tests assert on it.
	pub fn structure_digest(&self) -> String {
		let mut slots: Vec<u32> = self.slot_of.values().copied().collect();
		slots.sort_by(|a, b| self.id_of[*a as usize].cmp(&self.id_of[*b as usize]));
		let mut canon = format!(
			"ep={};max={}\n",
			self.ep.map(|s| self.id_str(s)).unwrap_or(""),
			self.max_layer
		);
		for slot in slots {
			let Some(node) = self.node(slot) else {
				continue;
			};
			canon.push_str(self.id_str(slot));
			for layer in &node.layers {
				canon.push('|');
				for &nb in layer {
					canon.push_str(self.id_str(nb));
					canon.push(',');
				}
			}
			canon.push('\n');
		}
		content_hash(&canon)
	}

	fn prune_neighbors(&self, center: u32, ids: &[u32], m: usize) -> Vec<u32> {
		let mut pairs: Vec<(u32, f64)> = ids
			.iter()
			.filter_map(|&id| {
				if self.node(id).is_some() {
					Some((id, self.distance_between(center, id)))
				} else {
					None
				}
			})
			.collect();
		pairs.sort_by(|a, b| cmp_partial(&a.1, &b.1).then_with(|| a.0.cmp(&b.0)));
		pairs.truncate(m);
		pairs.into_iter().map(|(id, _)| id).collect()
	}
}

// Equal distances break on slot id, so pop order never depends on push order.
trait HeapOrder {
	fn prefer(a: Candidate, b: Candidate) -> bool;
}
struct Min;
struct Max;
impl HeapOrder for Min {
	fn prefer(a: Candidate, b: Candidate) -> bool {
		a.dist < b.dist || (a.dist == b.dist && a.id < b.id)
	}
}
impl HeapOrder for Max {
	fn prefer(a: Candidate, b: Candidate) -> bool {
		a.dist > b.dist || (a.dist == b.dist && a.id > b.id)
	}
}

// Hand-rolled: the f64 distance sort key is not Ord, so std BinaryHeap won't do.
struct Heap<O: HeapOrder> {
	items: Vec<Candidate>,
	_order: PhantomData<O>,
}

impl<O: HeapOrder> Heap<O> {
	fn new() -> Self {
		Self {
			items: Vec::new(),
			_order: PhantomData,
		}
	}

	fn len(&self) -> usize {
		self.items.len()
	}

	fn peek(&self) -> Option<&Candidate> {
		self.items.first()
	}

	fn push(&mut self, c: Candidate) {
		self.items.push(c);
		let mut i = self.items.len() - 1;
		while i > 0 {
			let p = (i - 1) / 2;
			if !O::prefer(self.items[i], self.items[p]) {
				break;
			}
			self.items.swap(i, p);
			i = p;
		}
	}

	fn pop(&mut self) -> Option<Candidate> {
		if self.items.is_empty() {
			return None;
		}
		let n = self.items.len() - 1;
		self.items.swap(0, n);
		let top = self.items.pop().expect("non-empty checked above");
		let mut i = 0;
		let sz = self.items.len();
		loop {
			let (l, r) = (2 * i + 1, 2 * i + 2);
			let mut s = i;
			if l < sz && O::prefer(self.items[l], self.items[s]) {
				s = l;
			}
			if r < sz && O::prefer(self.items[r], self.items[s]) {
				s = r;
			}
			if s == i {
				break;
			}
			self.items.swap(i, s);
			i = s;
		}
		Some(top)
	}
}

type MinHeap = Heap<Min>;
type MaxHeap = Heap<Max>;

#[cfg(test)]
#[path = "tests/hnsw_test.rs"]
mod hnsw_tests;
