//! Seeding: the first candidate set for a query, fused from the content index,
//! the GNN-propagated index, and BM25 lexical hits per the mode's weights,
//! with importance fallbacks so an empty ANN result still seeds a walk.

use crate::retrieval::score::{matches_filter, QueryOptions};
use config::RetrievalConfig;
use graph::graph::GraphGnn;
use graph::lexical::LexicalIndex;
use graph::search::{
	search_all_filtered, search_all_unlocked, search_reasons_all_unlocked, EntityHit,
};
use math::cosine;
use rayon::iter::Either;
use rayon::prelude::*;
use std::collections::HashMap;

// Below this an in-kern split costs more than the walk it splits; see `seed_important`.
const PARALLEL_SCAN_MIN_ENTITIES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
	Content,
	Reason,
	Hybrid,
}

impl Mode {
	pub fn parse(s: &str) -> Self {
		match s.to_lowercase().as_str() {
			"content" => Self::Content,
			"reason" => Self::Reason,
			_ => Self::Hybrid,
		}
	}
}

#[derive(Debug, Clone, Copy)]
pub struct Weights {
	pub content: f64,
	pub reason: f64,
	pub edge: f64,
}

impl Weights {
	pub fn for_mode(cfg: &RetrievalConfig, m: Mode) -> Self {
		let w = match m {
			Mode::Content => cfg.weights_content,
			Mode::Reason => cfg.weights_reason,
			Mode::Hybrid => cfg.weights_hybrid,
		};
		Self {
			content: w.content,
			reason: w.reason,
			edge: w.edge,
		}
	}
}

pub fn seed_with_important(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	query_vec: &[f32],
	k: usize,
	mode: Mode,
	opts: Option<&QueryOptions>,
	important: &[EntityHit],
) -> Vec<EntityHit> {
	let mut hits = match mode {
		Mode::Reason => seed_by_reason(g, query_vec, k),
		// Filter DURING the ANN traversal so a sparse filter still yields k matching hits (not an unfiltered top-k post-filtered to fewer).
		_ => match opts {
			Some(o) if o.is_active() => {
				let keep = matches_keep(g, o);
				search_all_filtered(g, query_vec, k, &keep)
			}
			_ => search_all_unlocked(g, query_vec, k),
		},
	};
	hits = merge_seeds(hits, important.to_vec());
	hits.truncate(k.max(cfg.seed_k));
	hits
}

// The single filter shared by dense ANN, lexical, and post-filter, so they never diverge.
fn matches_keep<'a>(g: &'a GraphGnn, opts: &'a QueryOptions) -> impl Fn(&str) -> bool + 'a {
	move |id: &str| {
		g.kern_of_entity(id)
			.and_then(|kid| g.kerns.get(kid))
			.and_then(|kern| kern.entities.get(id))
			.is_some_and(|e| matches_filter(e, opts))
	}
}

pub fn seed_lexical(
	lex: &LexicalIndex,
	g: &GraphGnn,
	query_text: &str,
	k: usize,
	opts: Option<&QueryOptions>,
) -> Vec<EntityHit> {
	// Filter BEFORE the BM25 top-k truncation, so a sparse filter still yields k matching lexical hits.
	let raw = match opts {
		Some(o) if o.is_active() => lex.search_filtered(query_text, k, &matches_keep(g, o)),
		_ => lex.search(query_text, k),
	};
	raw
		.into_iter()
		.map(|h| EntityHit {
			entity_id: h.entity_id,
			score: h.score as f64,
		})
		.collect()
}

fn seed_by_reason(g: &GraphGnn, query_vec: &[f32], k: usize) -> Vec<EntityHit> {
	let reason_hits = search_reasons_all_unlocked(g, query_vec, k);
	let mut seen = HashMap::new();
	for rh in &reason_hits {
		let reason = g
			.kern_of_reason(&rh.reason_id)
			.and_then(|kid| g.loaded(kid))
			.and_then(|kern| kern.reasons.get(&rh.reason_id));
		if let Some(r) = reason {
			let entry = seen.entry(r.from.clone()).or_insert(0.0_f64);
			if rh.score > *entry {
				*entry = rh.score;
			}
		}
	}
	let mut hits: Vec<EntityHit> = seen.into_iter().map(EntityHit::from).collect();
	hits.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
	hits
}

pub fn seed_important(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	query_vec: &[f32],
	opts: Option<&QueryOptions>,
) -> Vec<EntityHit> {
	let kerns = g.all();
	let min_cos = cfg.important_min_cosine;
	let access_threshold = cfg.important_access_threshold;
	// Importance must respect an active filter at the SOURCE: non-matching important entities would crowd the merged seed and truncate matching ones out before the post-filter.
	let active_filter = opts.filter(|o| o.is_active());
	let mut hits: Vec<EntityHit> = kerns
		.par_iter()
		.flat_map(|kern| {
			let gate = move |t: &base::base_types::Entity| -> Option<EntityHit> {
				if !t.has_vector() {
					return None;
				}
				if let Some(o) = active_filter {
					if !matches_filter(t, o) {
						return None;
					}
				}
				let dominated = !t.is_fact() && t.access_count.value_i32() < access_threshold;
				if dominated {
					return None;
				}
				let score = cosine(query_vec, &t.vector);
				(score >= min_cos).then(|| EntityHit {
					entity_id: t.id.clone(),
					score,
				})
			};
			// `flat_map_iter` over `g.all()` parallelises over KERNS only, so the ordinary
			// single-kern corpus walked the whole graph on one thread however many cores
			// were free. Splitting inside the kern costs a fixed ~0.2 ms of rayon setup,
			// which on a small corpus exceeds the entire scan — measured 0.22x at N=1k and
			// 0.43x at N=10k against 1.6-2.8x at N=100k. So the split is earned by size,
			// and everything under the threshold keeps exactly the walk it already had.
			if kern.entities.len() >= PARALLEL_SCAN_MIN_ENTITIES {
				Either::Left(kern.entities.par_iter().filter_map(move |(_, t)| gate(t)))
			} else {
				Either::Right(
					kern
						.entities
						.values()
						.filter_map(gate)
						.collect::<Vec<_>>()
						.into_par_iter(),
				)
			}
		})
		.collect();
	hits.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
	hits
}

pub fn merge_seeds(a: Vec<EntityHit>, b: Vec<EntityHit>) -> Vec<EntityHit> {
	let scored = math::softmax_merge_scores(a.into_iter().chain(b).map(|h| (h.entity_id, h.score)));
	let mut out: Vec<EntityHit> = scored.into_iter().map(EntityHit::from).collect();
	out.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
	out
}

#[cfg(test)]
#[path = "tests/retrieval_seed_test.rs"]
mod retrieval_seed_tests;
