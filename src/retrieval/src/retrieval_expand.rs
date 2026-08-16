//! Graph-walk expansion: follow reason edges out from the seed hits, scoring
//! each hop by edge weight and query similarity, and keep the traversal as a
//! [`PathChain`] so every delivered result carries its provenance chain.

use crate::retrieval::seed::Weights;
use base::base_types::*;
use config::RetrievalConfig;
use graph::graph::GraphGnn;
use graph::search::EntityHit;
use math::cosine;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct PathChain {
	pub nodes: Vec<String>,
	pub score: f64,
}

#[derive(Debug, Clone)]
pub struct ScoredEntity {
	pub entity: Entity,
	pub score: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct ScoredRef<'a> {
	pub entity: &'a Entity,
	pub score: f64,
}

impl ScoredRef<'_> {
	pub fn to_owned(self) -> ScoredEntity {
		ScoredEntity {
			entity: self.entity.clone(),
			score: self.score,
		}
	}
}

pub trait Scored {
	fn entity(&self) -> &Entity;
	fn score(&self) -> f64;
	fn set_score(&mut self, score: f64);
}

impl Scored for ScoredEntity {
	fn entity(&self) -> &Entity {
		&self.entity
	}
	fn score(&self) -> f64 {
		self.score
	}
	fn set_score(&mut self, score: f64) {
		self.score = score;
	}
}

impl Scored for ScoredRef<'_> {
	fn entity(&self) -> &Entity {
		self.entity
	}
	fn score(&self) -> f64 {
		self.score
	}
	fn set_score(&mut self, score: f64) {
		self.score = score;
	}
}

pub struct ExpandResult<'a> {
	pub scored: Vec<ScoredRef<'a>>,
	pub chains: Vec<PathChain>,
}

#[derive(Default)]
struct Interner {
	idx: HashMap<Rc<str>, u32>,
	names: Vec<Rc<str>>,
}

impl Interner {
	fn intern(&mut self, s: &str) -> u32 {
		if let Some(&i) = self.idx.get(s) {
			return i;
		}
		let rc: Rc<str> = Rc::from(s);
		let i = self.names.len() as u32;
		self.names.push(Rc::clone(&rc));
		self.idx.insert(rc, i);
		i
	}

	fn name(&self, i: u32) -> &str {
		&self.names[i as usize]
	}

	// Rc<str> not &str so the caller can keep mutating the interner (interning neighbours) while holding this handle.
	fn name_rc(&self, i: u32) -> Rc<str> {
		Rc::clone(&self.names[i as usize])
	}
}

// A seed root has no edge (rid == "") and no parent (NO_PARENT).
struct ChainNode<'g> {
	ent: u32,
	rid: &'g str,
	parent: u32,
}

const NO_PARENT: u32 = u32::MAX;

struct BeamNode {
	ent: u32,
	score: f64,
	chain: u32,
}

// Max-heap keyed on score (assumed finite); ordering ignores the u32/arena payload.
#[derive(Default)]
struct Beam {
	items: Vec<BeamNode>,
}

impl Beam {
	fn push(&mut self, node: BeamNode) {
		self.items.push(node);
		let mut i = self.items.len() - 1;
		while i > 0 {
			let p = (i - 1) / 2;
			if self.items[i].score <= self.items[p].score {
				break;
			}
			self.items.swap(i, p);
			i = p;
		}
	}

	fn pop(&mut self) -> Option<BeamNode> {
		if self.items.is_empty() {
			return None;
		}
		let n = self.items.len() - 1;
		self.items.swap(0, n);
		let top = self.items.pop().unwrap();
		let sz = self.items.len();
		let mut i = 0;
		loop {
			let (l, r) = (2 * i + 1, 2 * i + 2);
			let mut s = i;
			if l < sz && self.items[l].score > self.items[s].score {
				s = l;
			}
			if r < sz && self.items[r].score > self.items[s].score {
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

fn materialize_chain(arena: &[ChainNode], interner: &Interner, mut node: u32) -> Vec<String> {
	let mut nodes: Vec<String> = Vec::new();
	loop {
		let n = &arena[node as usize];
		nodes.push(interner.name(n.ent).to_string());
		if n.parent == NO_PARENT {
			break;
		}
		nodes.push(n.rid.to_string());
		node = n.parent;
	}
	nodes.reverse();
	nodes
}

pub fn expand<'a>(
	g: &'a GraphGnn,
	cfg: &RetrievalConfig,
	query_vec: &'a [f32],
	seeds: &[EntityHit],
	w: Weights,
) -> ExpandResult<'a> {
	let mut interner = Interner::default();
	let mut heap = Beam::default();
	let mut arena: Vec<ChainNode> = Vec::new();
	let mut visited: HashSet<u32> = HashSet::new();
	let mut results: HashMap<u32, f64> = HashMap::new();
	let mut chains: Vec<PathChain> = Vec::new();
	// Traversal credit, kept OUTSIDE the max-per-entity walk score. `results`
	// keeps one max per entity, so when a neighbour is already a content hit its
	// seed score swallows the edge evidence — and pooling the two co-equally is
	// measured wrong: the best match pops first, is `visited`, and can never
	// receive hop evidence, so co-equal pooling systematically penalises the
	// direct answer. Instead every examined edge credits its far endpoint with
	// `source_score * edge_evidence`, once per (edge, endpoint) — the popping
	// side is credited by the same edge when the neighbour pops, which is what
	// lets a seed receive credit at all. Two bounds keep the walk from beating
	// direct matches: the summed credit is capped, and the credited total may
	// not reach the strongest crediting source's own walk score — a neighbour
	// rides up BEHIND what vouched for it, never past it, so a query's direct
	// answer cannot be outranked by its own neighbourhood.
	let mut credit: HashMap<u32, f64> = HashMap::new();
	let mut credit_src: HashMap<u32, f64> = HashMap::new();
	let mut credited: HashSet<(u32, u32)> = HashSet::new();
	// Best score SEEN AMONG NEIGHBOURS, never among seeds. Seed scores are a pure
	// query cosine (up to 1.0); a neighbour's is `w.content*cos + w.reason*cos +
	// w.edge*edge`, so with the default weights a neighbour the query does not
	// match directly cannot exceed w.reason + w.edge = 0.30. Pruning it against
	// `best_seed * decay` = 0.25 compared two different scales and killed the walk
	// whenever a seed matched well — which is the common case. Measured: a linked
	// pair scored 0.2411 against a 0.2500 threshold, so traversal contributed
	// nothing and linked/unlinked corpora ranked identically.
	let mut frontier_best: f64 = 0.0;

	for s in seeds {
		let ent = interner.intern(&s.entity_id);
		let chain = arena.len() as u32;
		arena.push(ChainNode {
			ent,
			rid: "",
			parent: NO_PARENT,
		});
		heap.push(BeamNode {
			ent,
			score: s.score,
			chain,
		});
	}

	let max_expansions = cfg.max_expansions;
	let decay = cfg.decay;
	let refine_tw = cfg.refine_traversal_weight;
	let refine_cap = cfg.refine_boost_cap;
	let credit_cap = cfg.traversal_credit_cap;
	let credit_weight = cfg.traversal_credit_weight;
	let mut expansions = 0;

	while let Some(item) = heap.pop() {
		if expansions >= max_expansions {
			break;
		}
		expansions += 1;

		if !visited.insert(item.ent) {
			continue;
		}

		let entry = results.entry(item.ent).or_insert(0.0);
		if item.score > *entry {
			*entry = item.score;
		}

		let threshold = frontier_best * decay;

		if arena[item.chain as usize].parent != NO_PARENT {
			chains.push(PathChain {
				nodes: materialize_chain(&arena, &interner, item.chain),
				score: item.score,
			});
		}

		let item_name = interner.name_rc(item.ent);
		let name: &str = &item_name;
		let Some((_thought, kern)) = find_entity_and_kern(g, name) else {
			continue;
		};
		let edges = kern
			.by_from
			.get(name)
			.into_iter()
			.flatten()
			.chain(kern.by_to.get(name).into_iter().flatten());
		for rid in edges {
			let Some(reason) = kern.reasons.get(rid) else {
				continue;
			};
			if reason.kind == ReasonKind::Spawn && !reason.to.is_empty() {
				continue;
			}
			let neighbor_id = if reason.from == name {
				reason.to.as_str()
			} else {
				reason.from.as_str()
			};
			if neighbor_id.is_empty() {
				continue;
			}
			let nu = interner.intern(neighbor_id);
			let evidence = edge_evidence(query_vec, reason, w, refine_tw, refine_cap);
			if evidence > 0.0 {
				let ru = interner.intern(rid);
				if credited.insert((ru, nu)) {
					// Linear source weighting, chosen by sweep 2026-07-21 against
					// source^2 and edge-reliability^2 variants: it was the only one
					// that IMPROVED recall@1 over the no-credit baseline (0.9306 vs
					// 0.9167) with equal multi-hop reach. The ceiling below, not the
					// weighting, is what protects direct answers.
					*credit.entry(nu).or_insert(0.0) += credit_weight * item.score * evidence;
					let src = credit_src.entry(nu).or_insert(0.0);
					if item.score > *src {
						*src = item.score;
					}
				}
			}
			if visited.contains(&nu) {
				continue;
			}
			let Some((neighbor, _)) = find_entity_and_kern(g, neighbor_id) else {
				continue;
			};
			let content_score = if neighbor.has_vector() {
				cosine(query_vec, &neighbor.vector)
			} else {
				0.0
			};
			let score = w.content * content_score + evidence;
			if score < threshold {
				continue;
			}
			// Only after it survives, so the first neighbour off any seed is always
			// explored and the bar is set by the frontier rather than by the seeds.
			if score > frontier_best {
				frontier_best = score;
			}
			let chain = arena.len() as u32;
			arena.push(ChainNode {
				ent: nu,
				rid: rid.as_str(),
				parent: item.chain,
			});
			heap.push(BeamNode {
				ent: nu,
				score,
				chain,
			});
		}
	}

	let scored: Vec<ScoredRef<'a>> = results
		.into_iter()
		.filter_map(|(id, score)| {
			let bonus = credit.get(&id).map_or(0.0, |c| c.min(credit_cap));
			let ceiling = credit_src
				.get(&id)
				.map_or(f64::INFINITY, |s| s - f64::EPSILON);
			let lifted = (score + bonus).min(ceiling).max(score);
			find_entity_and_kern(g, interner.name(id)).map(|(t, _)| ScoredRef {
				entity: t,
				score: lifted,
			})
		})
		.collect();

	ExpandResult { scored, chains }
}

// The query-conditioned evidence the edge itself supplies — everything in a
// neighbour's score except its own content match.
pub fn edge_evidence(
	query_vec: &[f32],
	reason: &Reason,
	w: Weights,
	refine_traversal_weight: f64,
	refine_boost_cap: f64,
) -> f64 {
	let reason_score = if reason.has_vector() {
		cosine(query_vec, &reason.vector)
	} else {
		0.0
	};
	let traversal_boost = ((reason.traversal_count.value() as f64 + 1.0).ln()
		* refine_traversal_weight)
		.min(refine_boost_cap);
	let edge_score = (reason.score.clamp(0.0, 1.0) + traversal_boost).min(1.0);

	w.reason * reason_score + w.edge * edge_score
}

pub fn score_neighbor(
	query_vec: &[f32],
	neighbor: &Entity,
	reason: &Reason,
	w: Weights,
	refine_traversal_weight: f64,
	refine_boost_cap: f64,
) -> f64 {
	let content_score = if neighbor.has_vector() {
		cosine(query_vec, &neighbor.vector)
	} else {
		0.0
	};
	w.content * content_score
		+ edge_evidence(
			query_vec,
			reason,
			w,
			refine_traversal_weight,
			refine_boost_cap,
		)
}

// Two-pass: O(1) via the kern_of_entity index, then a full scan fallback for stale/missing index entries.
fn find_entity_and_kern<'a>(g: &'a GraphGnn, id: &str) -> Option<(&'a Entity, &'a Kern)> {
	if let Some(kid) = g.kern_of_entity(id) {
		if let Some(kern) = g.loaded(kid) {
			if let Some(t) = kern.entities.get(id) {
				return Some((t, kern));
			}
		}
	}
	for kern in g.all() {
		if let Some(t) = kern.entities.get(id) {
			return Some((t, kern));
		}
	}
	None
}

pub fn find_entity_ref_in_graph<'a>(g: &'a GraphGnn, id: &str) -> Option<&'a Entity> {
	find_entity_and_kern(g, id).map(|(t, _)| t)
}

#[cfg(test)]
#[path = "tests/retrieval_expand_test.rs"]
mod retrieval_expand_tests;
