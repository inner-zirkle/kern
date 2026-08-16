//! Scoring policy: the confidence/fact/lexical boosts, query-based stigmergy
//! (access + recency), remote-trust down-weighting, the status/TTL/pending
//! filters, and the sort options — every rule that turns a similarity into a
//! deliverable rank.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::retrieval::expand::{Scored, ScoredEntity};
use base::base_constants::CONFIDENCE_BOUND_K;
use base::base_types::{Entity, EntityKind, EntityStatus, ReviewState};
use config::HeatConfig;
use config::RetrievalConfig;
use graph::graph::GraphGnn;
use graph::heat;
use graph::lexical::LexicalIndex;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use util::cmp_partial;
use util::LogThrottle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortField {
	#[default]
	Score,
	Date,
	Access,
	Confidence,
}

impl SortField {
	pub fn parse(s: &str) -> Self {
		match s.to_lowercase().as_str() {
			"date" => Self::Date,
			"access" => Self::Access,
			"confidence" => Self::Confidence,
			_ => Self::Score,
		}
	}
}

#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
	pub sort: SortField,
	pub ascending: bool,
	pub source: String,
	pub kind: Option<EntityKind>,
	pub scheme: Option<String>,
	pub since: Option<SystemTime>,
	pub before: Option<SystemTime>,
	pub min_conf: f64,
	pub valid_at: Option<SystemTime>,
	// WORLD-TIME point query (`[valid_from, valid_to)` covers this instant) — distinct from `valid_at`, which gates TTL expiry.
	pub as_of: Option<SystemTime>,
	// Superseded-history walk done at the tool layer, NOT a per-entity filter (the ANN never holds superseded entities).
	pub include_history: bool,
	// Drop entities still awaiting curation. OPT-IN: false is every caller that
	// names no review policy, so an uncurated graph reads exactly as before.
	pub exclude_pending: bool,
	// Multi-tenancy scoping. None = no filter on this dimension.
	pub user_id: Option<String>,
	pub agent_id: Option<String>,
	pub session_id: Option<String>,
	// Claim-kind label filter, pre-resolved at the tool layer to its subClassOf
	// closure (the label plus every registered descendant), so this predicate
	// stays a set-membership check with no graph access.
	pub claim_kinds: Option<Vec<String>>,
	// Appended to the synthesis prompt only — never a retrieval filter, so is_active() ignores it.
}

impl QueryOptions {
	pub fn is_active(&self) -> bool {
		!self.source.is_empty()
			|| self.kind.is_some()
			|| self.scheme.is_some()
			|| self.min_conf > 0.0
			|| self.since.is_some()
			|| self.before.is_some()
			|| self.valid_at.is_some()
			|| self.as_of.is_some()
			|| self.exclude_pending
			|| self.user_id.is_some()
			|| self.agent_id.is_some()
			|| self.session_id.is_some()
			|| self.claim_kinds.is_some()
	}
}

pub fn qbst(cfg: &RetrievalConfig, access_count: i32, accessed_at: Option<SystemTime>) -> f64 {
	let access = (access_count as f64 + 1.0).ln() * cfg.qbst_access_weight;
	let recency = match accessed_at {
		Some(at) => {
			let age = SystemTime::now()
				.duration_since(at)
				.unwrap_or_default()
				.as_secs_f64();
			let half_life = Duration::from_secs(cfg.qbst_recency_half_life_secs)
				.as_secs_f64()
				.max(1.0);
			cfg.qbst_recency_weight * (-age / half_life).exp()
		}
		None => 0.0,
	};
	(access + recency).min(cfg.qbst_cap)
}

/// Late-fusion BM25 bonus: add `cfg.lexical_top_boost * (bm25 / max_bm25)` to
/// each delivered result's score, using the query's own BM25 ranking over the
/// corpus. Normalized by the top BM25 score so the bonus is 0..1 * weight and
/// comparable across corpora of different sizes. A no-op when the weight is 0
/// or no result has a BM25 score (verbatim query terms absent from the corpus).
/// Runs before gravity/filter/MMR, so an exact-lexical match wins the top.
pub fn apply_lexical_boost<T: Scored>(
	lex: &LexicalIndex,
	cfg: &RetrievalConfig,
	query_text: &str,
	results: &mut [T],
) {
	if cfg.lexical_top_boost <= 0.0 || results.is_empty() {
		return;
	}
	let hits = lex.search(query_text, results.len());
	if hits.is_empty() {
		return;
	}
	let max = hits
		.iter()
		.map(|h| h.score)
		.fold(0.0f32, f32::max)
		.max(1e-9);
	let bm25: HashMap<&str, f32> = hits
		.iter()
		.map(|h| (h.entity_id.as_str(), h.score))
		.collect();
	for r in results.iter_mut() {
		let norm = (*bm25.get(r.entity().id.as_str()).unwrap_or(&0.0) / max) as f64;
		r.set_score(r.score() + cfg.lexical_top_boost * norm);
	}
}

/// Scale each result by its confidence and add the flat fact bonus.
pub fn apply_boosts<T: Scored>(cfg: &RetrievalConfig, results: &mut [T]) {
	for r in results.iter_mut() {
		let e = r.entity();
		// Lower confidence bound, not the mean: a well-evidenced claim outranks a
		// single-observation one at equal mean (ROADMAP item 65). Clamped >= 0 so
		// a high-variance claim never inverts the boost.
		let confidence = (e.conf_mean() - CONFIDENCE_BOUND_K * e.conf_variance().sqrt()).max(0.0);
		let boost = qbst(cfg, e.access_count.value_i32(), e.accessed_at);
		let fact_bonus = if e.kind == EntityKind::Fact {
			cfg.fact_score_boost
		} else {
			0.0
		};
		let trust = cfg
			.source_trust
			.get(e.source.scheme())
			.copied()
			.unwrap_or(1.0);
		r.set_score((r.score() * confidence + boost + fact_bonus) * trust);
	}
}

// A thought's access count and heat may be reinforced at most once per window.
// Retrieval stamps every delivered result, so without this a caller replaying one
// query pumps a single thought's rank for free. Sized to
// collapse a burst while leaving genuine reuse across a working session
// countable; heat's half-life is measured in days, so a minute costs nothing real.
const ACCESS_COOLDOWN: Duration = Duration::from_secs(60);

const BELOW_FLOOR_WARN_SECS: u64 = 60;
static BELOW_FLOOR: AtomicU64 = AtomicU64::new(0);
static BELOW_FLOOR_WARN: LogThrottle = LogThrottle::new(BELOW_FLOOR_WARN_SECS);

// Deliveries that bypassed `min_deliver_score` because nothing cleared it. The
// caller cannot tell such a result from a good one, so the count is its trace.
pub fn below_floor_deliveries() -> u64 {
	BELOW_FLOOR.load(Ordering::Relaxed)
}

// Bi-temporal expiry on EVERY delivery, not only when a caller thinks to pass
// `valid_at`. Until this ran unconditionally, `valid_until` was near-dead code —
// honoured by `matches_filter` alone, whose only caller was the MCP `valid_at`
// param — so an expired claim still ranked on the default recall path and
// "bi-temporal supersede off the recall path" was true only of the write path.
//
// Skipped when the query names an instant of its own: a point-in-time query
// judges validity AT that instant, so a claim that has since expired is exactly
// what it should return. `valid_at` is already enforced by `matches_filter`.
pub fn drop_expired<T: Scored>(results: &mut Vec<T>, opts: Option<&QueryOptions>, now: SystemTime) {
	if opts.is_some_and(|o| o.as_of.is_some() || o.valid_at.is_some()) {
		return;
	}
	results.retain(|r| r.entity().valid_until.is_none_or(|exp| exp >= now));
}

pub fn filter_delivery<T: Scored>(cfg: &RetrievalConfig, results: &mut Vec<T>) {
	results.retain(|r| r.entity().status != EntityStatus::Superseded);
	// Sort HERE, not just in apply_query_options: the truncation below is the delivery
	// cut, so it has to see post-boost order. Without this every boost, gravity pull and
	// trust penalty is invisible whenever no QueryOptions is supplied.
	results.sort_by(|a, b| util::cmp_rank(a.score(), &a.entity().id, b.score(), &b.entity().id));
	let floor = cfg.min_deliver_score;
	if results.iter().any(|r| r.score() >= floor) {
		results.retain(|r| r.score() >= floor);
	} else if !results.is_empty() {
		// Deliberate: a query whose entire candidate set is below the quality floor
		// returns that set rather than nothing, so recall degrades instead of going
		// blank. But an unflagged bypass is indistinguishable from a confident
		// answer, which is the whole complaint in ROADMAP item 7 — count it.
		let total = BELOW_FLOOR.fetch_add(1, Ordering::Relaxed) + 1;
		if BELOW_FLOOR_WARN.allow() {
			tracing::warn!(
				target: "kern.retrieval",
				floor,
				best = results.first().map(|r| r.score()).unwrap_or(0.0),
				candidates = results.len(),
				total_bypasses = total,
				"no candidate cleared min_deliver_score — delivering the below-floor set \
				 rather than nothing (further bypasses counted, not logged)"
			);
		}
	}
	results.truncate(delivery_cap(cfg));
}

/// How many results a query may deliver.
///
/// One owner, because two callers need it: `filter_delivery` cuts the pool with
/// it, and the CLI has to ask a serving daemon for exactly this many. Without
/// that, `kern query` silently returns `seed_k` hits when a daemon is up and the
/// full delivery pool when one is not — the same command, two answers.
///
/// With MMR on, the larger MMR pool is kept: truncating to the delivery cap here
/// would make MMR's len-guard a no-op.
pub fn delivery_cap(cfg: &RetrievalConfig) -> usize {
	if cfg.mmr_enabled {
		cfg.mmr_pool_size.max(cfg.max_deliver_results)
	} else {
		cfg.max_deliver_results
	}
}

// Single filter predicate shared by post-filtering and pre-filtered ANN search (`search_all_filtered`) — the two must never diverge.
pub fn matches_filter(entity: &Entity, opts: &QueryOptions) -> bool {
	if opts.exclude_pending && entity.review == ReviewState::Pending {
		return false;
	}
	if !opts.source.is_empty() && entity.source.system() != opts.source {
		return false;
	}
	if let Some(want) = opts.kind {
		if entity.kind != want {
			return false;
		}
	}
	if let Some(ref want) = opts.scheme {
		if entity.source.scheme() != want.as_str() {
			return false;
		}
	}
	if let Some(ref want) = opts.claim_kinds {
		// Only distilled claims carry a claim-kind label, as the Session title
		// `session://<kind>`; everything else reads as the empty label and drops.
		let label = entity
			.source
			.title()
			.strip_prefix("session://")
			.unwrap_or("");
		if !want.iter().any(|w| w == label) {
			return false;
		}
	}
	if opts.min_conf > 0.0 && entity.score < opts.min_conf {
		return false;
	}
	if let Some(since) = opts.since {
		if entity.created_at.is_some_and(|t| t < since) {
			return false;
		}
	}
	if let Some(before) = opts.before {
		if entity.created_at.is_some_and(|t| t > before) {
			return false;
		}
	}
	if let Some(valid_at) = opts.valid_at {
		if entity.valid_until.is_some_and(|exp| exp < valid_at) {
			return false;
		}
	}
	if let Some(as_of) = opts.as_of {
		if !entity.is_valid_at(as_of) {
			return false;
		}
	}
	if let Some(ref want) = opts.user_id {
		if entity.user_id.as_deref() != Some(want.as_str()) {
			return false;
		}
	}
	if let Some(ref want) = opts.agent_id {
		if entity.agent_id.as_deref() != Some(want.as_str()) {
			return false;
		}
	}
	if let Some(ref want) = opts.session_id {
		if entity.session_id.as_deref() != Some(want.as_str()) {
			return false;
		}
	}
	true
}

pub fn apply_query_options<T: Scored>(results: &mut Vec<T>, opts: &QueryOptions) {
	results.retain(|r| matches_filter(r.entity(), opts));

	let asc = opts.ascending;
	let dir = |ord: std::cmp::Ordering| if asc { ord } else { ord.reverse() };
	match opts.sort {
		SortField::Score => {
			results.sort_by(|a, b| dir(cmp_partial(&a.score(), &b.score())));
		}
		SortField::Date => {
			results.sort_by(|a, b| dir(a.entity().created_at.cmp(&b.entity().created_at)));
		}
		SortField::Access => {
			results.sort_by(|a, b| {
				dir(
					a.entity()
						.access_count
						.value()
						.cmp(&b.entity().access_count.value()),
				)
			});
		}
		SortField::Confidence => {
			results.sort_by(|a, b| dir(cmp_partial(&a.entity().score, &b.entity().score)));
		}
	}
}

pub fn commit_access(results: &mut [ScoredEntity], heat_cfg: &HeatConfig) {
	let now = SystemTime::now();
	for r in results.iter_mut() {
		stamp_access(&mut r.entity, now, heat_cfg);
	}
}

// Every delivered result is stamped, so replaying one query would otherwise pump
// a single thought's count and heat without bound. Both are ranking signals, and
// "retrieval learns from use" has to mean sustained use, not repetition. A
// future `accessed_at` (rewound clock) is not treated as throttled — heat decay
// already handles skew, and freezing the counter there would be a second bug.
//
// Returns false when the stamp was suppressed, so a caller can skip the work it
// would otherwise do on the back of it.
fn stamp_access(e: &mut Entity, now: SystemTime, heat_cfg: &HeatConfig) -> bool {
	let throttled = e
		.accessed_at
		.is_some_and(|last| now.duration_since(last).is_ok_and(|d| d < ACCESS_COOLDOWN));
	if throttled {
		return false;
	}
	let replica = if e.producer_id.is_empty() {
		"local"
	} else {
		e.producer_id.as_str()
	};
	e.access_count.increment(replica, 1);
	e.accessed_at = Some(now);
	e.heat = heat::deposit_for(e, now, heat_cfg.half_life_secs, heat_cfg.deposit_access);
	e.heat_updated_at = Some(now);
	true
}

// Goes through `kerns` directly, NOT `get_mut`: an access stamp must not bump the mutation epoch (it would invalidate the query cache).
pub fn commit_access_ids(g: &mut GraphGnn, ids: &[String], heat_cfg: &HeatConfig) {
	let now = SystemTime::now();
	for id in ids {
		let Some(kern_id) = g.kern_of_entity(id).map(str::to_string) else {
			continue;
		};
		if let Some(e) = g
			.kerns
			.get_mut(&kern_id)
			.and_then(|k| k.entities.get_mut(id))
		{
			stamp_access(e, now, heat_cfg);
		}
	}
}

#[cfg(test)]
#[path = "tests/retrieval_score_test.rs"]
mod retrieval_score_tests;
