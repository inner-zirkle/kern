//! Graph-walk expansion: follow reason edges out from the seed hits, scoring
//! each hop by edge weight and query similarity, and keep the traversal as a
//! [`PathChain`] so every delivered result carries its provenance chain.

use crate::base_types::*;
use crate::config::RetrievalConfig;
use crate::graph::GraphGnn;
use crate::math::cosine;
use crate::retrieval::seed::Weights;
use crate::search::EntityHit;
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
			if reason.is_remote() {
				continue;
			}
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
mod tests {
	use super::*;
	use crate::reason::add_reason;

	use crate::test_support::entity_vec as ent;
	fn edge(from: &str, to: &str, score: f64) -> Reason {
		Reason {
			id: format!("{from}->{to}"),
			from: from.into(),
			to: to.into(),
			score,
			kind: ReasonKind::Similarity,
			..Default::default()
		}
	}

	#[test]
	fn score_neighbor_pure_content_weight_is_cosine() {
		let neighbor = ent("n", vec![1.0, 0.0]);
		let r = edge("a", "n", 0.5);
		let w = Weights {
			content: 1.0,
			reason: 0.0,
			edge: 0.0,
		};
		let s = score_neighbor(&[1.0, 0.0], &neighbor, &r, w, 0.1, 0.3);
		assert!(
			(s - 1.0).abs() < 1e-9,
			"query aligned with neighbour -> 1.0"
		);
	}

	#[test]
	fn score_neighbor_pure_edge_weight_uses_clamped_reason_score() {
		let neighbor = ent("n", vec![]);
		let r = edge("a", "n", 0.4);
		let w = Weights {
			content: 0.0,
			reason: 0.0,
			edge: 1.0,
		};
		let s = score_neighbor(&[1.0, 0.0], &neighbor, &r, w, 0.1, 0.3);
		assert!(
			(s - 0.4).abs() < 1e-9,
			"edge component is the clamped reason score"
		);
	}

	#[test]
	fn expand_walks_edges_from_seed_and_records_a_chain() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		for id in ["a", "b", "c"] {
			k.entities.insert(id.into(), ent(id, vec![1.0, 0.0]));
		}
		add_reason(&mut k, edge("a", "b", 0.9));
		add_reason(&mut k, edge("b", "c", 0.9));
		g.kerns.insert("kx".into(), k);

		let cfg = RetrievalConfig::default();
		let seeds = [EntityHit {
			entity_id: "a".into(),
			score: 1.0,
		}];
		let w = Weights {
			content: 1.0,
			reason: 0.0,
			edge: 0.0,
		};
		let res = expand(&g, &cfg, &[1.0, 0.0], &seeds, w);

		let ids: HashSet<&str> = res.scored.iter().map(|s| s.entity.id.as_str()).collect();
		assert!(ids.contains("a"), "the seed is scored");
		assert!(
			ids.contains("b"),
			"the 1-hop neighbour is reached via the edge"
		);
		assert!(
			res.chains.iter().any(|c| c.nodes.len() >= 3),
			"a multi-hop chain (entity, reason, entity) is recorded"
		);
	}
	fn linked_pair_graph() -> GraphGnn {
		// a matches the query [1,0] exactly; b is orthogonal, reachable only
		// across the edge. Mirrors the ROADMAP item 86 measurement: b is also a
		// (weak) content hit, so the max-per-entity walk score alone gives the
		// edge no way to move it.
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("a".into(), ent("a", vec![1.0, 0.0]));
		k.entities.insert("b".into(), ent("b", vec![0.0, 1.0]));
		let mut r = edge("a", "b", 0.9);
		r.vector = vec![0.7, 0.7].into();
		add_reason(&mut k, r);
		g.kerns.insert("kx".into(), k);
		g
	}

	const PAIR_WEIGHTS: Weights = Weights {
		content: 0.70,
		reason: 0.15,
		edge: 0.15,
	};

	fn pair_seeds() -> [EntityHit; 2] {
		[
			EntityHit {
				entity_id: "a".into(),
				score: 1.0,
			},
			EntityHit {
				entity_id: "b".into(),
				score: 0.0,
			},
		]
	}

	fn score_of(res: &ExpandResult, id: &str) -> f64 {
		res
			.scored
			.iter()
			.find(|s| s.entity.id == id)
			.unwrap_or_else(|| panic!("{id} missing from scored"))
			.score
	}

	#[test]
	fn an_edge_off_a_strong_seed_lifts_a_neighbour_that_is_already_a_weak_hit() {
		let g = linked_pair_graph();
		let cfg = RetrievalConfig::default();
		let res = expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS);

		let evidence = 0.15 * (0.7 / (0.7f32 * 0.7 + 0.7 * 0.7).sqrt() as f64) + 0.15 * 0.9;
		let b = score_of(&res, "b");
		assert!(
			b > evidence + 1e-6,
			"b must carry credit ON TOP of its walk score, got {b} vs evidence {evidence}"
		);
		let a = score_of(&res, "a");
		assert!(
			a > b,
			"the direct match still outranks the lifted neighbour"
		);
	}

	#[test]
	fn credit_from_a_weaker_voucher_cannot_lift_past_the_voucher() {
		// b pops at its edge-derived walk score and credits a back across the
		// same edge, but a already outranks b — the ceiling annuls the lift, so
		// the direct answer's score is exactly its walk score, not walk + bonus.
		let g = linked_pair_graph();
		let cfg = RetrievalConfig::default();
		let res = expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS);

		let a = score_of(&res, "a");
		assert!(
			(a - 1.0).abs() < 1e-9,
			"credit sourced below the seed must not move it, got {a}"
		);
	}

	#[test]
	fn a_lifted_neighbour_saturates_just_below_its_strongest_voucher() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("c".into(), ent("c", vec![1.0, 0.0]));
		k.entities.insert("n".into(), ent("n", vec![0.6, 0.8]));
		let mut r = edge("c", "n", 1.0);
		r.vector = vec![1.0, 0.0].into();
		add_reason(&mut k, r);
		g.kerns.insert("kx".into(), k);

		let cfg = RetrievalConfig {
			traversal_credit_cap: 1.0,
			..Default::default()
		};
		let seeds = [
			EntityHit {
				entity_id: "c".into(),
				score: 1.0,
			},
			EntityHit {
				entity_id: "n".into(),
				score: 0.6,
			},
		];
		let res = expand(&g, &cfg, &[1.0, 0.0], &seeds, PAIR_WEIGHTS);

		let (c, n) = (score_of(&res, "c"), score_of(&res, "n"));
		assert!(
			n < c,
			"the lifted neighbour stays behind its voucher: n={n} c={c}"
		);
		assert!(
			n > 0.9,
			"but the ceiling, not the cap, is what stopped it: n={n}"
		);
	}

	#[test]
	fn traversal_credit_is_capped() {
		let g = linked_pair_graph();
		let mut cfg = RetrievalConfig {
			traversal_credit_cap: 0.0,
			..Default::default()
		};
		let off = score_of(
			&expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS),
			"b",
		);

		cfg.traversal_credit_cap = 0.01;
		let capped = score_of(
			&expand(&g, &cfg, &[1.0, 0.0], &pair_seeds(), PAIR_WEIGHTS),
			"b",
		);

		assert!(
			(capped - (off + 0.01)).abs() < 1e-9,
			"bonus must saturate at the cap: off={off} capped={capped}"
		);
	}

	#[test]
	fn a_strong_seed_no_longer_prunes_the_walk_off_it() {
		// The seed scale (pure query cosine, up to 1.0) and the neighbour scale
		// (0.70*content + 0.15*reason + 0.15*edge, so at most 0.30 for a neighbour
		// the query does not match) are different scales. Thresholding one against
		// the other killed traversal whenever a seed matched well.
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("a".into(), ent("a", vec![1.0, 0.0]));
		// Orthogonal to the query: reachable only across the edge.
		k.entities.insert("b".into(), ent("b", vec![0.0, 1.0]));
		let mut r = edge("a", "b", 0.9);
		r.vector = vec![0.7, 0.7].into();
		add_reason(&mut k, r);
		g.kerns.insert("kx".into(), k);

		let cfg = RetrievalConfig::default();
		let seeds = [EntityHit {
			entity_id: "a".into(),
			score: 1.0,
		}];
		let w = Weights {
			content: 0.70,
			reason: 0.15,
			edge: 0.15,
		};
		let res = expand(&g, &cfg, &[1.0, 0.0], &seeds, w);

		let ids: HashSet<&str> = res.scored.iter().map(|s| s.entity.id.as_str()).collect();
		assert!(
			ids.contains("b"),
			"a neighbour off a perfectly-matching seed must still be walked; \
			 got {ids:?}"
		);
		assert!(
			!res.chains.is_empty(),
			"and the walk must be recorded as a chain"
		);
	}
}
