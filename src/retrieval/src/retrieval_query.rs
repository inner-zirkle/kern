//! The query orchestrator: seed → expand → merge → boost → gravity → trust →
//! filter → diversify, in that order, returning scored entities with their
//! provenance chains. The one place the pipeline's stage order is spelled out;
//! every stage lives in its own `retrieval_*` module.

use crate::retrieval::expand::{
	self, find_entity_ref_in_graph, PathChain, Scored, ScoredEntity, ScoredRef,
};
use crate::retrieval::score::QueryOptions;
use crate::retrieval::seed::{Mode, Weights};
use crate::retrieval::{diversify, pagerank, score, seed};
use base::base_constants::QUERY_MAX_CHAINS;
use config::HeatConfig;
use config::RetrievalConfig;
use graph::graph::GraphGnn;
use graph::search::{find_entity, find_reason};
use util::profile::Profiler;

#[derive(Debug, Clone)]
pub struct QueryResult {
	pub entities: Vec<ScoredEntity>,
	pub path_chains: Vec<PathChain>,
}

pub fn query(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	heat_cfg: &HeatConfig,
	query_vec: &[f32],
	query_text: &str,
	mode: Mode,
	opts: Option<QueryOptions>,
) -> QueryResult {
	let (result, profile) = query_profiled(g, cfg, heat_cfg, query_vec, query_text, mode, opts);
	tracing::debug!(target: "kern.profile", "{}", profile);
	result
}

pub fn query_profiled(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	heat_cfg: &HeatConfig,
	query_vec: &[f32],
	query_text: &str,
	mode: Mode,
	opts: Option<QueryOptions>,
) -> (QueryResult, util::profile::Profile) {
	let mut prof = Profiler::new("query");
	let w = Weights::for_mode(cfg, mode);

	let Retrieved {
		mut results,
		chains,
		chain_text: _,
	} = retrieve(g, cfg, query_vec, query_text, mode, opts.as_ref(), w);
	prof.checkpoint("retrieve");

	score::commit_access(&mut results, heat_cfg);

	(
		QueryResult {
			entities: results,
			path_chains: chains,
		},
		prof.finish(),
	)
}

// chain_text is pre-rendered while the graph lock is held, so delivery needs no graph access afterward.
pub struct Retrieved {
	pub results: Vec<ScoredEntity>,
	pub chains: Vec<PathChain>,
	pub chain_text: String,
}

#[allow(clippy::too_many_arguments)]
fn fuse_hybrid_seeds(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	opts: Option<&QueryOptions>,
	lex: &graph::lexical::LexicalIndex,
	qvec: &[f32],
	dense_seeds: Vec<graph::search::EntityHit>,
	query_text: &str,
	imp_hits: &[graph::search::EntityHit],
) -> Vec<graph::search::EntityHit> {
	let lex_hits = seed::seed_lexical(lex, g, query_text, cfg.seed_k * 4, opts);
	let pr_hits = if cfg.pagerank_enabled {
		// Teleport personalized at dense + lexical seeds only — importance is query-independent and would make PageRank query-blind.
		let ppr_seeds: Vec<graph::search::EntityHit> =
			dense_seeds.iter().chain(lex_hits.iter()).cloned().collect();
		pagerank::pagerank(
			g,
			&ppr_seeds,
			cfg.pagerank_damping,
			cfg.pagerank_iters,
			cfg.pagerank_top_k,
		)
	} else {
		Vec::new()
	};
	let gw = cfg.rrf_global_weight;
	// Intent biases the three query-shaped lists only; PageRank keeps the plain
	// global weight — authority is intent-blind. A General classification (or
	// the knob off) is all-1.0, bit-identical to the pre-intent fusion.
	let intent = if cfg.intent_enabled {
		let i = crate::retrieval::intent::classify_intent(query_text);
		if i.category != crate::retrieval::intent::IntentCategory::General {
			tracing::debug!(
				target: "kern.retrieval",
				intent = i.category.as_str(),
				confidence = i.confidence,
				"query intent biases the hybrid fusion"
			);
		}
		i
	} else {
		crate::retrieval::intent::QueryIntent::general()
	};
	let mut lists: Vec<&[graph::search::EntityHit]> = vec![&dense_seeds, &lex_hits, imp_hits];
	let mut weights: Vec<f64> = vec![
		intent.dense_bias,
		intent.lexical_bias,
		gw * intent.importance_bias,
	];
	if !pr_hits.is_empty() {
		lists.push(&pr_hits);
		weights.push(gw);
	}
	let mut fused = rrf(&lists, &weights, cfg.rrf_k, cfg.seed_k.max(1) * 2);
	// RRF decides WHICH entities seed; it must not decide how much they score.
	// Its reciprocal-rank scores live on a ~1/rrf_k scale while expand() scores
	// neighbours on the cosine scale — pooled in merge_results(), any expanded neighbour
	// outscored every seed and ranking inverted. Rescore the fused survivors by
	// query cosine so seeds re-enter the pipeline on the one scale it speaks.
	for h in &mut fused {
		h.score = expand::find_entity_ref_in_graph(g, &h.entity_id)
			.map(|e| math::cosine(qvec, &e.vector))
			.unwrap_or(0.0);
	}
	fused.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
	fused
}

// The whole read path (seed -> expand -> merge -> score -> diversify). NO LLM,
// ever — this is the single endpoint the instrument tunes, and the calling
// agent owns synthesis.
pub fn retrieve(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	qvec: &[f32],
	query_text: &str,
	mode: Mode,
	opts: Option<&QueryOptions>,
	w: Weights,
) -> Retrieved {
	retrieve_profiled(g, cfg, qvec, query_text, mode, opts, w).0
}

#[allow(clippy::too_many_arguments)]
pub fn retrieve_profiled(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	qvec: &[f32],
	query_text: &str,
	mode: Mode,
	opts: Option<&QueryOptions>,
	w: Weights,
) -> (Retrieved, util::profile::Profile) {
	let mut prof = Profiler::new("retrieve");
	let lexical = g.lexical();
	let lex_ref = lexical.as_deref();
	// The O(N) importance scan feeds both the dense-seed merge and the RRF list — run once here, threaded into both.
	let important = seed::seed_important(g, cfg, qvec, opts);
	let dense_seeds = seed::seed_with_important(g, cfg, qvec, cfg.seed_k, mode, opts, &important);
	prof.checkpoint("seed_dense");

	let seeds = if mode == Mode::Hybrid && cfg.lexical_enabled && !query_text.is_empty() {
		match lex_ref {
			Some(lex) => fuse_hybrid_seeds(g, cfg, opts, lex, qvec, dense_seeds, query_text, &important),
			None => dense_seeds,
		}
	} else {
		dense_seeds
	};
	prof.checkpoint("fuse_hybrid");

	if seeds.is_empty() {
		return (
			Retrieved {
				results: Vec::new(),
				chains: Vec::new(),
				chain_text: String::new(),
			},
			prof.finish(),
		);
	}

	let expanded = expand::expand(g, cfg, qvec, &seeds, w);
	prof.checkpoint("expand");
	let mut results = merge_results(g, &seeds, expanded.scored);
	let mut chains = expanded.chains;
	prof.checkpoint("merge");

	score::apply_boosts(cfg, &mut results);
	apply_gravity(g, cfg, &mut results);
	// An active filter must run BEFORE filter_delivery's pool truncation, or expansion's non-matching neighbours crowd matching entities out of the cap.
	if let Some(o) = opts {
		if o.is_active() {
			results.retain(|r| score::matches_filter(r.entity, o));
			// A chain is rendered by `format_chains` as the TEXT of every entity on
			// it. Filtering only `results` leaves the chain rendering as a second
			// delivery channel that answers no filter at all. A path through a
			// filtered entity is dropped whole: a chain with a hole in it would
			// still say the filtered thought exists and what it connects.
			chains.retain(|c| {
				c.nodes.iter().step_by(2).all(|id| {
					expand::find_entity_ref_in_graph(g, id).is_none_or(|e| score::matches_filter(e, o))
				})
			});
		}
	}
	score::drop_expired(&mut results, opts, std::time::SystemTime::now());
	score::filter_delivery(cfg, &mut results);

	if let Some(opts) = opts {
		score::apply_query_options(&mut results, opts);
	}

	diversify::dedup_by_section(cfg, &mut results);
	prof.checkpoint("boosts+filter");
	diversify::mmr(cfg, qvec, &mut results);
	prof.checkpoint("mmr");

	// Late-fusion lexical boost applied AFTER MMR: MMR's relevance term is raw
	// query-cosine, so any score bonus added before it is invisible whenever the
	// pool exceeds `max_deliver_results`. Pinning exact-lexical matches to the
	// top of the delivered list must re-sort the final order, post-diversity.
	if cfg.lexical_top_boost > 0.0 {
		if let Some(lex) = lex_ref {
			score::apply_lexical_boost(lex, cfg, query_text, &mut results);
			results.sort_by(|a, b| util::cmp_rank(a.score, &a.entity.id, b.score, &b.entity.id));
		}
	}

	let results: Vec<ScoredEntity> = results.into_iter().map(ScoredRef::to_owned).collect();
	prof.checkpoint("materialize");

	let chain_text = format_chains(g, &chains);
	prof.checkpoint("chains");
	(
		Retrieved {
			results,
			chains,
			chain_text,
		},
		prof.finish(),
	)
}

// Holds the read lock for ONLY the graph phase. Daemon MCP path; plain query() serves the one-shot CLI.
pub fn query_locked(
	graph: &parking_lot::RwLock<GraphGnn>,
	cfg: &RetrievalConfig,
	heat_cfg: &HeatConfig,
	query_vec: &[f32],
	query_text: &str,
	mode: Mode,
	opts: Option<QueryOptions>,
) -> QueryResult {
	let w = Weights::for_mode(cfg, mode);

	let mut retrieved = {
		let g = graph.read();
		retrieve(&g, cfg, query_vec, query_text, mode, opts.as_ref(), w)
	};

	score::commit_access(&mut retrieved.results, heat_cfg);
	// Live-graph access write-back is deferred to a CommitAccess tick task (see
	// mcp::Server::tool_query) so this path takes ONLY a read lock.

	QueryResult {
		entities: retrieved.results,
		path_chains: retrieved.chains,
	}
}

pub fn format_chains(g: &GraphGnn, chains: &[PathChain]) -> String {
	let mut out = String::new();
	for (i, chain) in chains.iter().take(QUERY_MAX_CHAINS).enumerate() {
		out.push_str(&format!("Chain {}:\n", i + 1));
		for (j, node_id) in chain.nodes.iter().enumerate() {
			if j % 2 == 0 {
				if let Some((t, _)) = find_entity(g, node_id) {
					let text = util::truncate(&t.text(), 200);
					out.push_str(&format!("  [Entity] {text}\n"));
				}
			} else if let Some((r, _)) = find_reason(g, node_id) {
				let label = if !r.text.is_empty() {
					util::truncate(&r.text, 100).to_string()
				} else if let Some(lbl) = r.kind.fallback_label() {
					lbl.to_string()
				} else {
					continue;
				};
				out.push_str(&format!("  --{label}-->\n"));
			}
		}
	}
	out
}

// ==== [fuse] ====

use graph::search::EntityHit;
use std::collections::HashMap;

pub fn rrf(lists: &[&[EntityHit]], weights: &[f64], k_rrf: f64, top_k: usize) -> Vec<EntityHit> {
	let mut agg: HashMap<String, f64> = HashMap::new();
	for (li, list) in lists.iter().enumerate() {
		let w = weights.get(li).copied().unwrap_or(1.0);
		for (i, hit) in list.iter().enumerate() {
			let rank = (i + 1) as f64;
			let contrib = w / (k_rrf + rank);
			*agg.entry(hit.entity_id.clone()).or_insert(0.0) += contrib;
		}
	}
	if top_k == 0 {
		return Vec::new();
	}
	let mut out: Vec<EntityHit> = agg.into_iter().map(EntityHit::from).collect();
	// Unique ids make this a STRICT total order, so the top_k partition + sorting only the survivors equals a full sort + truncate.
	let cmp =
		|a: &EntityHit, b: &EntityHit| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id);
	if top_k < out.len() {
		out.select_nth_unstable_by(top_k - 1, &cmp);
		out.truncate(top_k);
	}
	out.sort_by(&cmp);
	out
}

// ==== [merge] ====

use math::OnlineSoftmax;

// Log-sum-exp score pool: an entity in both sources earns +ln(count). Result is a magnitude, not a probability — may exceed 1.0.
pub fn merge_results<'a>(
	g: &'a GraphGnn,
	seeds: &[EntityHit],
	beam: Vec<ScoredRef<'a>>,
) -> Vec<ScoredRef<'a>> {
	let mut scores: HashMap<&str, OnlineSoftmax> = HashMap::new();
	let mut thoughts: HashMap<&str, ScoredRef<'a>> = HashMap::new();

	for st in beam {
		scores.entry(&st.entity.id).or_default().update(st.score);
		thoughts.entry(&st.entity.id).or_insert(st);
	}

	for s in seeds {
		if let Some(t) = thoughts.get(s.entity_id.as_str()) {
			scores.entry(&t.entity.id).or_default().update(s.score);
		} else if let Some(t) = find_entity_ref_in_graph(g, &s.entity_id) {
			scores.entry(&t.id).or_default().update(s.score);
			thoughts.insert(
				&t.id,
				ScoredRef {
					entity: t,
					score: s.score,
				},
			);
		}
	}

	let mut results: Vec<ScoredRef<'a>> = thoughts
		.into_iter()
		.filter_map(|(id, mut st)| {
			let merged = scores.get(id)?.finalize();
			st.score = merged;
			Some(st)
		})
		.collect();

	// Score desc, id asc — the id tie-break is required for determinism (HashMap order varies per process).
	results.sort_by(|a, b| util::cmp_rank(a.score, &a.entity.id, b.score, &b.entity.id));
	results
}

// ==== [gravity] ====

use base::base_types::Kern;
use graph::accept::root_graviton_ids;
use math::cosine;

// Max over gravitons, not sum — overlapping gravitons must not double-count.
pub fn apply_gravity<T: Scored>(g: &GraphGnn, cfg: &RetrievalConfig, results: &mut [T]) {
	if cfg.gravity_weight == 0.0 {
		return;
	}
	let gravitons: Vec<&Kern> = root_graviton_ids(g)
		.into_iter()
		.filter_map(|id| g.loaded(&id))
		.filter(|k| !k.graviton_vec.is_empty())
		.collect();
	if gravitons.is_empty() {
		return;
	}
	for r in results.iter_mut() {
		let vec = &r.entity().vector;
		if vec.is_empty() {
			continue;
		}
		let pull = gravitons
			.iter()
			.map(|k| k.mass * cosine(&k.graviton_vec, vec).max(0.0))
			.fold(0.0_f64, f64::max);
		if pull > 0.0 {
			r.set_score(r.score() + cfg.gravity_weight * pull);
		}
	}
}

#[cfg(test)]
#[path = "tests/retrieval_query_test.rs"]
mod retrieval_query_tests;
