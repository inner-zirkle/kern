//! The daemon's core server: the graph, worker, LLM client, task queue and
//! config a daemon owns, plus the operation surface the CLI drives over the
//! typed RPC. Every operation returns plain JSON — no MCP envelope.

use std::sync::Arc;

use parking_lot::RwLock;

use config::Config;
use graph::graph::GraphGnn;

pub type PulseBroadcast = Arc<dyn Fn(&str, f64) + Send + Sync>;

pub struct Server {
	pub graph: Arc<RwLock<GraphGnn>>,
	pub worker: Arc<ingest::Worker>,
	pub llm: Option<llm::Client>,
	pub save_fn: Arc<dyn Fn() + Send + Sync>,
	pub task_q: Option<Arc<tick::tick_queue::Queue>>,
	pub cfg: Arc<Config>,
	pub broadcast_pulse: Option<PulseBroadcast>,
	// Epoch ms of the last real operation (health polls excluded, or the hub's
	// own idle probe would keep every node alive forever). Seeded at boot so a
	// never-used node counts idle from startup.
	pub last_activity: Arc<std::sync::atomic::AtomicU64>,
	pub query_cache: QueryCache,
}

/// Per-daemon query cache, two tiers:
/// - **embeddings**, keyed on query text alone — graph-independent, so a
///   repeat query skips the network round-trip to the embed endpoint (the
///   dominant per-query cost) even right after an ingest;
/// - **results**, keyed on the full argument JSON and guarded by the graph's
///   `mutation_epoch` — any content mutation bumps the epoch and every cached
///   result goes stale at once. Access stamps deliberately do NOT bump it, so
///   cache hits survive reads; a hit still re-enqueues its CommitAccess task
///   so heat keeps measuring use.
#[derive(Default)]
pub struct QueryCache {
	embeds: parking_lot::Mutex<std::collections::HashMap<String, Vec<f32>>>,
	results: parking_lot::Mutex<std::collections::HashMap<String, CachedResult>>,
	hits: std::sync::atomic::AtomicU64,
	misses: std::sync::atomic::AtomicU64,
}

struct CachedResult {
	epoch: u64,
	// The delivered ids, kept so a hit can still deposit access heat.
	ids: Vec<String>,
	value: serde_json::Value,
}

// Bounds are a backstop, not an eviction policy: overflow clears the map whole,
// which is deterministic and cheap, and a working set past the cap means the
// caller is sweeping, not repeating — nothing worth ranking for retention.
const EMBED_CACHE_CAP: usize = 256;
const RESULT_CACHE_CAP: usize = 128;

impl QueryCache {
	pub fn embed_get(&self, text: &str) -> Option<Vec<f32>> {
		self.embeds.lock().get(text).cloned()
	}

	pub fn embed_put(&self, text: &str, vec: Vec<f32>) {
		let mut m = self.embeds.lock();
		if m.len() >= EMBED_CACHE_CAP {
			m.clear();
		}
		m.insert(text.to_string(), vec);
	}

	pub fn result_get(&self, key: &str, epoch: u64) -> Option<(serde_json::Value, Vec<String>)> {
		let m = self.results.lock();
		match m.get(key) {
			Some(c) if c.epoch == epoch => {
				self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
				Some((c.value.clone(), c.ids.clone()))
			}
			_ => {
				self
					.misses
					.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
				None
			}
		}
	}

	pub fn result_put(&self, key: String, epoch: u64, ids: Vec<String>, value: serde_json::Value) {
		let mut m = self.results.lock();
		if m.len() >= RESULT_CACHE_CAP {
			m.clear();
		}
		m.insert(key, CachedResult { epoch, ids, value });
	}

	pub fn hits(&self) -> u64 {
		self.hits.load(std::sync::atomic::Ordering::Relaxed)
	}

	pub fn misses(&self) -> u64 {
		self.misses.load(std::sync::atomic::Ordering::Relaxed)
	}
}

impl Server {
	pub fn idle_ms(&self) -> u64 {
		let last = self
			.last_activity
			.load(std::sync::atomic::Ordering::Relaxed);
		util::now_ms().saturating_sub(last)
	}

	pub(crate) fn touch(&self) {
		self
			.last_activity
			.store(util::now_ms(), std::sync::atomic::Ordering::Relaxed);
	}

	/// Dispatch one named operation with JSON args. The single surface the CLI
	/// drives; every operation returns plain JSON or a string error.
	pub fn invoke(&self, name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		if name != "health" {
			self.touch();
		}
		match name {
			"query" => self.tool_query(args),
			"search" => self.tool_search(args),
			"log" => self.tool_log(args),
			"events" => self.tool_events(args),
			"ingest" => self.tool_ingest(args),
			"link" => self.tool_link(args),
			"forget" => self.tool_forget(args),
			"forget_by_source" => self.tool_forget_by_source(args),
			"degrade" => self.tool_degrade(args),
			"move" => self.tool_move(args),
			"promote" => self.tool_promote(args),
			"health" => self.tool_health(),
			"graviton" => self.tool_graviton(args),
			"claim_kind" => self.tool_claim_kind(args),
			"pulse" => self.tool_pulse(args),
			"gc" => self.tool_gc(),
			"audit" => self.tool_audit(args),
			"intake_drain" => self.tool_intake_drain(),
			"setup" => self.tool_setup(),
			_ => Err(format!("unknown operation: {name}")),
		}
	}
}

#[derive(Default)]
struct TickHealth {
	queue_depth: u64,
	tasks_done: u64,
	task_avg_ms: u64,
	task_panics: u64,
	last_task_panic: Option<String>,
	task_failures: u64,
	last_task_failure: Option<String>,
}

impl TickHealth {
	fn of(q: &Arc<tick::tick_queue::Queue>) -> Self {
		let (done, avg_ms) = q.metrics();
		let (task_panics, last_panic) = q.panics();
		let (task_failures, last_failure) = q.failures();
		Self {
			queue_depth: q.pending_count() as u64,
			tasks_done: done.max(0) as u64,
			task_avg_ms: avg_ms.max(0) as u64,
			task_panics,
			last_task_panic: last_panic.map(|p| p.to_string()),
			task_failures,
			last_task_failure: last_failure.map(|f| f.to_string()),
		}
	}
}

impl Server {
	pub(crate) fn health_stats(&self) -> serde_json::Value {
		let g = self.graph.read();
		let h = ::health::graph_health_stats(&g);
		let claim_kinds = g.root.claim_kinds.len();
		let tick = self.task_q.as_ref().map(TickHealth::of).unwrap_or_default();
		serde_json::json!({
			// The daemon's own store path: what `hub status` sizes and `kern
			// status` names. Config-sourced — the graph's copy is load-derived.
			"data_dir": self.cfg.data_dir,
			"gravitons": h.gravitons,
			"kerns": h.kerns,
			"entities": h.entities,
			"reasons": h.reasons,
			"unnamed": h.unnamed,
			"claim_kinds": claim_kinds,
			"queue_depth": tick.queue_depth,
			"tasks_done": tick.tasks_done,
			"task_avg_ms": tick.task_avg_ms,
			"task_panics": tick.task_panics,
			"last_task_panic": tick.last_task_panic,
			"task_failures": tick.task_failures,
			"last_task_failure": tick.last_task_failure,
			"cold_evicted": h.cold_evicted,
			"embed_model": h.embed_model,
			"embed_dim": h.embed_dim,
			"embed_mismatch": h.embed_mismatch,
			"query_dim_rejected": h.query_dim_rejected,
			"below_floor_deliveries": h.below_floor_deliveries,
			"clock_skew_skips": h.clock_skew_skips,
			"ingest_dropped_chunks": h.ingest_dropped_chunks,
			"unspilled_drops": h.unspilled_drops,
			"ingest_queue_refused": h.ingest_queue_refused,
			"ingest_hygiene_rejected": h.ingest_hygiene_rejected,
			// Per-daemon, reset on restart. A hit skipped the embed round-trip
			// and the whole retrieval pipeline; misses count first-sights and
			// epoch invalidations alike.
			"query_cache_hits": self.query_cache.hits(),
			"query_cache_misses": self.query_cache.misses(),
			// 0.0 = uniform access (converged); →1.0 = one entity holds all
			// access. Resident entities only.
			"gini_access": h.gini_access,
			// The resident-kern cap: u64::MAX = uncapped (KERN_CAP_DISABLED).
			"max_kerns": h.max_kerns as u64,
			// Supersede chains past `SUPERSEDE_CHAIN_HOP_THRESHOLD` on one
			// `external_id`.
			"supersede_chain_depth_exceeded": h.supersede_chain_depth_exceeded,
			// Largest resident kern's entity count: a gauge of the unbounded
			// resident set at per-kern granularity.
			"largest_kern_entities": h.largest_kern_entities,
			// Gini over resident kern sizes: the distribution the
			// `largest_kern_entities` max summarises — kern-size balance.
			"gini_kern_sizes": h.gini_kern_sizes,
			// Active heat retention half-life (HeatConfig.half_life_secs; the one
			// Preset::apply sets, never a config edit). Daemon-sourced like the
			// config-derived fields — the CLI's own config is irrelevant.
			"heat_half_life_secs": self.cfg.heat.half_life_secs,
			// QBST recency half-life — the 24h ranking-freshness signal, the
			// second of the two freshness signals (the heat half-life above is
			// the first). Daemon-sourced.
			"qbst_recency_half_life_secs": self.cfg.retrieval.qbst_recency_half_life_secs,
			// Active RRF config + mode blends. Preset-owned; surfaced
			// daemon-sourced so an operator sees which preset's retrieval runs.
			"retrieval": {
				"rrf_k": self.cfg.retrieval.rrf_k,
				"rrf_global_weight": self.cfg.retrieval.rrf_global_weight,
				"weights_content": {
					"content": self.cfg.retrieval.weights_content.content,
					"reason": self.cfg.retrieval.weights_content.reason,
					"edge": self.cfg.retrieval.weights_content.edge,
				},
				"weights_reason": {
					"content": self.cfg.retrieval.weights_reason.content,
					"reason": self.cfg.retrieval.weights_reason.reason,
					"edge": self.cfg.retrieval.weights_reason.edge,
				},
				"weights_hybrid": {
					"content": self.cfg.retrieval.weights_hybrid.content,
					"reason": self.cfg.retrieval.weights_hybrid.reason,
					"edge": self.cfg.retrieval.weights_hybrid.edge,
				},
				"seed_k": self.cfg.retrieval.seed_k,
				"mmr_enabled": self.cfg.retrieval.mmr_enabled,
				"lexical_enabled": self.cfg.retrieval.lexical_enabled,
				"pagerank_enabled": self.cfg.retrieval.pagerank_enabled,
			},
			// Active preset name. Preset::apply is its only writer; the name
			// frames the heat/recency/retrieval lines.
			"preset": self.cfg.preset.as_str(),
			// Active source-trust map, keyed on `Source::scheme()`. Empty by
			// default (bit-identical scoring), so an unconfigured kern surfaces
			// an empty map. Daemon-sourced.
			"source_trust": self.cfg.retrieval.source_trust,
			// Active ingest dedup config: the global `dedup_threshold` plus the
			// per-kind `dedup_threshold_by_kind` array. `None` falls back to the
			// global. Daemon-sourced.
			"ingest_dedup_threshold": self.cfg.ingest.dedup_threshold,
			"ingest_dedup_threshold_by_kind": self.cfg.ingest.dedup_threshold_by_kind,
			// This server's own worker, read directly: a gauge on the live
			// channel, not a process static like the counters `h` carries.
			"ingest_queue_depth": self.worker.queue_depth(),
			"gnn_train_refused": tick_loop::tick_trainer::gnn_train_refused(),
			// Read straight from the client, like `gnn_train_refused` above: it
			// is a property of this process's LLM leg, not of the graph `h`
			// describes.
			"llm_complete_failed": llm::complete_failed(),
			"last_llm_complete_failure": llm::last_complete_failure(),
		})
	}
}

// ==== query ====

use serde::Deserialize;

use base::base_types::EntityKind;
use retrieval::id_detail::{base_entity_json, resolve_by_id};
use util::truncate;

fn parse_time_filter(field: &str, value: &str) -> Result<Option<std::time::SystemTime>, String> {
	if value.is_empty() {
		return Ok(None);
	}
	util::parse_rfc3339(value)
		.map(Some)
		.map_err(|()| format!("invalid `{field}` timestamp: {value}"))
}

fn build_query_options(p: &QueryArgs) -> Result<retrieval::score::QueryOptions, String> {
	let mut opts = retrieval::score::QueryOptions {
		sort: retrieval::score::SortField::parse(&p.sort),
		ascending: p.ascending,
		source: p.source.clone(),
		kind: p.kind,
		min_conf: p.min_conf,
		since: parse_time_filter("since", &p.since)?,
		before: parse_time_filter("before", &p.before)?,
		valid_at: parse_time_filter("valid_at", &p.valid_at)?,
		as_of: parse_time_filter("as_of", &p.as_of)?,
		include_history: p.include_history,
		exclude_pending: p.exclude_pending,
		..Default::default()
	};
	if let Some(ref s) = p.scheme {
		match base::base_types::Source::parse_scheme(s) {
			Some(tag) => opts.scheme = Some(tag.to_string()),
			None => return Err(format!("unknown source scheme: {s}")),
		}
	}
	Ok(opts)
}

#[derive(Deserialize, Default)]
struct QueryArgs {
	#[serde(default)]
	text: String,
	#[serde(default)]
	id: String,
	#[serde(default)]
	ids: Vec<String>,
	#[serde(default)]
	k: usize,
	#[serde(default)]
	mode: String,
	#[serde(default)]
	sort: String,
	#[serde(default)]
	ascending: bool,
	#[serde(default)]
	source: String,
	#[serde(default, deserialize_with = "de_kind")]
	kind: Option<EntityKind>,
	#[serde(default)]
	claim_kind: String,
	#[serde(default)]
	scheme: Option<String>,
	#[serde(default)]
	since: String,
	#[serde(default)]
	before: String,
	#[serde(default)]
	min_conf: f64,
	#[serde(default)]
	valid_at: String,
	#[serde(default)]
	as_of: String,
	#[serde(default)]
	include_history: bool,
	#[serde(default)]
	exclude_pending: bool,
}

// The filter takes the stable lowercase labels (`EntityKind::as_str`), not the
// Rust variant names serde derive would expect.
fn de_kind<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<EntityKind>, D::Error> {
	let s = Option::<String>::deserialize(d)?;
	match s.as_deref() {
		None | Some("") => Ok(None),
		Some(v) => EntityKind::parse(v)
			.map(Some)
			.ok_or_else(|| serde::de::Error::custom(format!("unknown kind: {v}"))),
	}
}

impl Server {
	// Resolves a `claim_kind` label to its subClassOf closure against the live
	// registry, so `matches_filter` stays graph-free. Unknown labels are an
	// error, not an empty result — a typo should say so.
	fn apply_claim_kind_filter(
		&self,
		p: &QueryArgs,
		opts: &mut retrieval::score::QueryOptions,
	) -> Result<(), String> {
		if p.claim_kind.is_empty() {
			return Ok(());
		}
		let g = self.graph.read();
		let known = ingest::distill::DEFAULT_KINDS.contains(&p.claim_kind.as_str())
			|| g.root.claim_kinds.contains_key(&p.claim_kind);
		if !known {
			return Err(format!("unknown claim kind: {}", p.claim_kind));
		}
		opts.claim_kinds = Some(g.root.claim_kind_closure(&p.claim_kind));
		Ok(())
	}

	#[allow(clippy::field_reassign_with_default)]
	pub(crate) fn tool_query(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: QueryArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};

		if !p.ids.is_empty() {
			// Batch direct lookup: same filters as `id`, applied per row. Returns
			// `{results, missing}` so a caller can tell a filter-drop (in results,
			// flagged) from a non-existent id (in missing). Prefix and cold tier
			// both resolve, matching the single-id path.
			let mut opts = build_query_options(&p)?;
			self.apply_claim_kind_filter(&p, &mut opts)?;
			let g = self.graph.read();
			let mut results = Vec::new();
			let mut missing = Vec::new();
			for id in &p.ids {
				match resolve_by_id(&g, id)
					.filter(|hit| retrieval::score::matches_filter(&hit.thought, &opts))
				{
					Some(hit) => results.push(hit.detail(&g)),
					None => missing.push(id.clone()),
				}
			}
			return Ok(serde_json::json!({"results": results, "missing": missing}));
		}

		if !p.id.is_empty() {
			// The same filters the ranked read honours, applied to the one row an
			// id names: `query {id, kind: "claim"}` that answered with a Fact would
			// make the filter mean one thing on `text` and nothing on `id`.
			// A bare `query {id}` still serves everything — `QueryOptions::default()`
			// leaves every filter off, `valid_at`/`as_of` included, so an expired row
			// keeps arriving flagged rather than filtered.
			let mut opts = build_query_options(&p)?;
			self.apply_claim_kind_filter(&p, &mut opts)?;
			let g = self.graph.read();
			// Prefix and cold tier both included so `kern get` can route here
			// without resolving fewer ids than it did reading the store itself.
			let hit = resolve_by_id(&g, &p.id)
				.filter(|hit| retrieval::score::matches_filter(&hit.thought, &opts));
			return match hit {
				Some(hit) => Ok(hit.detail(&g)),
				None => Err(format!("thought not found: {}", p.id)),
			};
		}

		if p.text.is_empty() {
			return Err("either text, id or ids is required".to_string());
		}

		let llm = match &self.llm {
			Some(c) => c.clone(),
			None => return Err("no embed client configured".to_string()),
		};

		let mode = retrieval::seed::Mode::parse(&p.mode);
		let rcfg = &self.cfg.retrieval;

		// Result cache: keyed on the raw argument JSON (text, mode, k, every
		// filter), valid only for the epoch it was computed under. A hit skips
		// the embed round-trip AND the whole pipeline, but still deposits the
		// access heat its ids earned — a cached read is still a read.
		let cache_key = util::content_hash(&format!("query\x00{args}"));
		let epoch = self.graph.read().mutation_epoch();
		if let Some((value, ids)) = self.query_cache.result_get(&cache_key, epoch) {
			if let Some(ref q) = self.task_q {
				if !ids.is_empty() {
					q.enqueue(tick::tick_queue::task_commit_access(&ids));
				}
			}
			return Ok(value);
		}

		let vec = match self.query_cache.embed_get(&p.text) {
			Some(v) => v,
			None => match llm::block_on_in_place(llm.embed(&p.text)) {
				Some(Ok(v)) => {
					self.query_cache.embed_put(&p.text, v.clone());
					v
				}
				Some(Err(e)) => return Err(format!("embed failed: {e}")),
				None => return Err("no tokio runtime".to_string()),
			},
		};

		let mut opts = build_query_options(&p)?;
		self.apply_claim_kind_filter(&p, &mut opts)?;
		let opts = opts;

		let result = retrieval::query::query_locked(
			&self.graph,
			rcfg,
			&self.cfg.heat,
			&vec,
			&p.text,
			mode,
			Some(opts.clone()),
		);
		// query_locked took only a read lock; access stamps commit off the hot
		// path via CommitAccess (advisory, skipped without a queue).
		if let Some(ref q) = self.task_q {
			let ids: Vec<String> = result
				.entities
				.iter()
				.map(|s| s.entity.id.clone())
				.collect();
			if !ids.is_empty() {
				q.enqueue(tick::tick_queue::task_commit_access(&ids));
			}
		}
		let vec = Some(vec);
		(self.save_fn)();

		let k = if p.k == 0 { rcfg.seed_k } else { p.k };

		let mut scored: Vec<retrieval::expand::ScoredEntity> = result.entities.clone();
		let mut cold_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
		// Exact-text fast path skipped embedding (`vec` None), so cold-tier fill is skipped too.
		if let Some(ref vec) = vec {
			if scored.len() < k {
				// Clone the store handle under a brief read guard; drop it before the scan.
				let store = self.graph.read().store();
				let have: std::collections::HashSet<String> =
					scored.iter().map(|s| s.entity.id.clone()).collect();
				if let Some(store) = &store {
					for (entity, score) in store.cold_search(vec, k).unwrap_or_default() {
						if scored.len() >= k {
							break;
						}
						// cold_search is a raw cosine scan of the spill tier — it answers no
						// filter. Delivering its hits unfiltered made the cold tier a way
						// around every predicate the hot path enforces.
						if !retrieval::score::matches_filter(&entity, &opts) {
							continue;
						}
						if !have.contains(&entity.id) {
							cold_ids.insert(entity.id.clone());
							scored.push(retrieval::expand::ScoredEntity { entity, score });
						}
					}
				}
			}
		}

		// The ANN never holds Superseded rows; walk Supersedes chains back from the
		// active hits for history.
		let mut history_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
		if p.include_history {
			// The same `opts` the ranked read and the cold fill used — rebuilding it
			// here was a second chance for the three paths to disagree about what the
			// caller asked for.
			let g = self.graph.read();
			let heads: Vec<(String, f64)> = scored
				.iter()
				.map(|s| (s.entity.id.clone(), s.score))
				.collect();
			let mut have: std::collections::HashSet<String> =
				scored.iter().map(|s| s.entity.id.clone()).collect();
			for (head_id, head_score) in heads {
				for anc_id in graph::reason::superseded_ancestors(&g, &head_id) {
					if !have.insert(anc_id.clone()) {
						continue;
					}
					let ancestor = g
						.kern_of_entity(&anc_id)
						.and_then(|kid| g.kerns.get(kid))
						.and_then(|k| k.entities.get(&anc_id))
						.cloned()
						.or_else(|| g.store().and_then(|s| s.cold_get(&anc_id).ok().flatten()));
					if let Some(ent) = ancestor {
						if retrieval::score::matches_filter(&ent, &opts) {
							history_ids.insert(anc_id.clone());
							scored.push(retrieval::expand::ScoredEntity {
								entity: ent,
								score: head_score,
							});
						}
					}
				}
			}
		}

		let take_n = k + history_ids.len();
		let entities: Vec<serde_json::Value> = {
			let g = self.graph.read();
			scored
				.iter()
				.take(take_n)
				.map(|st| {
					let edges: Vec<serde_json::Value> = g
						.kern_of_entity(&st.entity.id)
						.and_then(|kid| g.kerns.get(kid))
						.map(|kern| {
							graph::reason::collect_reason_ids(kern, &st.entity.id)
								.into_iter()
								.filter_map(|rid| kern.reasons.get(&rid))
								.filter(|r| r.is_enriched())
								.map(|r| {
									serde_json::json!({
										"from": r.from,
										"to": r.to,
										"kind": r.kind as i32,
										"text": truncate(&r.text, 120),
										"score": r.score,
									})
								})
								.collect()
						})
						.unwrap_or_default();
					let mut v = base_entity_json(&st.entity, st.score);
					v["cold"] = serde_json::Value::Bool(cold_ids.contains(&st.entity.id));
					if history_ids.contains(&st.entity.id) {
						v["history"] = serde_json::Value::Bool(true);
					}
					if !edges.is_empty() {
						v["edges"] = serde_json::Value::Array(edges);
					}
					v
				})
				.collect()
		};

		let chains = {
			let g = self.graph.read();
			retrieval::query::format_chains(&g, &result.path_chains)
		};

		let response = serde_json::json!({"entities": entities, "chains": chains});
		// Stored under the epoch captured BEFORE the read: if an ingest landed
		// mid-query the stored entry is already stale and the next get misses.
		let delivered_ids: Vec<String> = scored
			.iter()
			.take(take_n)
			.map(|s| s.entity.id.clone())
			.collect();
		self
			.query_cache
			.result_put(cache_key, epoch, delivered_ids, response.clone());
		Ok(response)
	}
}

// ==== search (cross-kern) ====

#[derive(Deserialize, Default)]
struct SearchArgs {
	#[serde(default)]
	text: String,
	#[serde(default)]
	k: u64,
	#[serde(default)]
	live_only: bool,
}

impl Server {
	// The machine-wide read: hand the query to the hub, which fans it out to
	// every registered kern and merges by score. No hub running means no other
	// kerns are reachable — answer from this graph alone and say so, because a
	// silent narrowing would read exactly like a machine with one project.
	pub(crate) fn tool_search(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: SearchArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};
		if p.text.is_empty() {
			return Err("text is required".to_string());
		}

		use transport::hub_rpc::{HubRpcClient, SearchReq};
		use transport::typed::JsonEnvelopeCodec;

		let req = SearchReq {
			text: p.text.clone(),
			k: p.k,
			live_only: p.live_only,
		};
		let via_hub = llm::block_on_in_place(async move {
			let hub = HubRpcClient::<JsonEnvelopeCodec>::connect_hub()
				.await
				.ok()?;
			hub.search(req).await.ok()
		})
		.flatten();

		match via_hub {
			Some(res) if res.ok => {
				let hits: Vec<serde_json::Value> = res
					.hits
					.into_iter()
					.map(|h| serde_json::json!({"root": h.root, "entity": h.entity}))
					.collect();
				let skipped: Vec<serde_json::Value> = res
					.skipped
					.into_iter()
					.map(|s| serde_json::json!({"root": s.root, "err": s.err}))
					.collect();
				Ok(serde_json::json!({"fanout": true, "hits": hits, "skipped": skipped}))
			}
			Some(res) => Err(res.err),
			// No hub: answer from this graph, flagged, so the caller can tell a
			// one-project machine from an unreachable hub.
			None => {
				let local = self.tool_query(&serde_json::json!({"text": p.text, "k": p.k}))?;
				let root = std::env::current_dir()
					.map(|p| p.display().to_string())
					.unwrap_or_default();
				let hits: Vec<serde_json::Value> = local
					.get("entities")
					.and_then(|v| v.as_array())
					.cloned()
					.unwrap_or_default()
					.into_iter()
					.map(|entity| serde_json::json!({"root": root, "entity": entity}))
					.collect();
				Ok(serde_json::json!({
					"fanout": false,
					"note": "no machine hub reachable — results are this project only (start one with `kern hub`)",
					"hits": hits,
					"skipped": [],
				}))
			}
		}
	}
}

// ==== log ====

const LOG_DEFAULT_LIMIT: usize = 20;

#[derive(Deserialize, Default)]
struct LogArgs {
	#[serde(default)]
	id: String,
	#[serde(default)]
	limit: usize,
}

impl Server {
	pub(crate) fn tool_log(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: LogArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};
		let g = self.graph.read();
		let id = (!p.id.is_empty()).then_some(p.id.as_str());
		log_report(&g, id, p.limit)
	}
}

/// The git-porcelain read, shared by the daemon operation and the CLI's
/// no-daemon fallback so the two cannot disagree about what history means.
/// `id = None`: the machine history — every added/superseded change the
/// bitemporal stamps record, newest first, capped at `limit`. `id = Some`:
/// that thought's revision chain (head first, then the Supersedes walk), each
/// revision with its source, stamps, and the supersede reason as the why.
pub fn log_report(
	g: &GraphGnn,
	id: Option<&str>,
	limit: usize,
) -> Result<serde_json::Value, String> {
	let limit = if limit == 0 { LOG_DEFAULT_LIMIT } else { limit };
	match id {
		Some(id) => log_chain(g, id),
		None => Ok(log_history(g, limit)),
	}
}

fn source_uri(src: &base::base_types::Source) -> String {
	let mut s = format!("{}://{}", src.scheme(), src.object_id());
	if !src.section().is_empty() {
		s.push('#');
		s.push_str(src.section());
	}
	s
}

fn log_history(g: &GraphGnn, limit: usize) -> serde_json::Value {
	struct Row {
		at: SystemTime,
		// created before superseded on an (impossible in practice) equal stamp,
		// same tie-break the events feed pins.
		ord: u8,
		id: String,
		change: &'static str,
		kind: &'static str,
		scheme: &'static str,
		text: String,
	}
	let mut rows: Vec<Row> = Vec::new();
	for kern in g.all() {
		for e in kern.entities.values() {
			if let Some(created) = e.created_at {
				rows.push(Row {
					at: created,
					ord: 0,
					id: e.id.clone(),
					change: "added",
					kind: e.kind.as_str(),
					scheme: e.source.scheme(),
					text: truncate(&e.text(), 120),
				});
			}
			if e.is_superseded() {
				if let Some(inv) = e.invalidated_at {
					rows.push(Row {
						at: inv,
						ord: 1,
						id: e.id.clone(),
						change: "superseded",
						kind: e.kind.as_str(),
						scheme: e.source.scheme(),
						text: truncate(&e.text(), 120),
					});
				}
			}
		}
	}
	rows.sort_by(|a, b| {
		b.at
			.cmp(&a.at)
			.then(b.ord.cmp(&a.ord))
			.then(a.id.cmp(&b.id))
	});
	rows.truncate(limit);
	let events: Vec<serde_json::Value> = rows
		.into_iter()
		.map(|r| {
			serde_json::json!({
				"id": r.id,
				"at": util::datetime_string(r.at),
				"change": r.change,
				"kind": r.kind,
				"scheme": r.scheme,
				"text": r.text,
			})
		})
		.collect();
	serde_json::json!({ "events": events })
}

fn log_chain(g: &GraphGnn, id: &str) -> Result<serde_json::Value, String> {
	let Some(hit) = resolve_by_id(g, id) else {
		return Err(format!("thought not found: {id}"));
	};
	let mut chain = vec![hit.thought.id.clone()];
	chain.extend(graph::reason::superseded_ancestors(g, &hit.thought.id));

	let entity_of = |rid: &str| {
		find_entity(g, rid)
			.map(|(e, _)| e)
			.or_else(|| g.store().and_then(|s| s.cold_get(rid).ok().flatten()))
	};
	// The why lives on the Supersedes edge the NEWER revision points at the
	// older one with; its text is the recorded rationale.
	let why_of = |newer: &str, older: &str| -> String {
		find_entity(g, newer)
			.and_then(|(_, kid)| g.kerns.get(&kid))
			.and_then(|kern| {
				let edges = kern.by_from.get(newer)?;
				edges.iter().find_map(|rid| {
					let r = kern.reasons.get(rid)?;
					(r.kind == base::base_types::ReasonKind::Supersedes && r.to == older)
						.then(|| r.text.clone())
				})
			})
			.filter(|t| !t.is_empty())
			.unwrap_or_default()
	};

	let mut revisions = Vec::new();
	for (i, rid) in chain.iter().enumerate() {
		let Some(e) = entity_of(rid) else {
			continue;
		};
		let why = if i == 0 {
			String::new()
		} else {
			why_of(&chain[i - 1], rid)
		};
		revisions.push(serde_json::json!({
			"id": e.id,
			"status": if e.is_superseded() { "superseded" } else { "active" },
			"kind": e.kind.as_str(),
			"scheme": e.source.scheme(),
			"source": source_uri(&e.source),
			"created": e.created_at.map(util::datetime_string).unwrap_or_default(),
			"invalidated": e.invalidated_at.map(util::datetime_string).unwrap_or_default(),
			"why": why,
			"text": e.text(),
		}));
	}
	Ok(serde_json::json!({ "revisions": revisions }))
}

// The porcelain contract: what entered is listed newest-first, and a
// superseded revision's chain carries its stamps and its why.
#[cfg(test)]
mod log_tests {
	use super::*;
	use base::base_types::{Kern, Reason, ReasonKind};

	fn stamped(id: &str, text: &str, secs: u64) -> base::base_types::Entity {
		let mut e = test_support::entity(id);
		e.set_text(text.to_string());
		e.created_at = Some(std::time::UNIX_EPOCH + Duration::from_secs(secs));
		e
	}

	#[tokio::test]
	async fn history_lists_newest_first_and_honours_the_cap() {
		let srv = crate::test_helpers::server();
		{
			let mut g = srv.graph.write();
			let mut k = Kern::new("kx", "");
			for (i, id) in ["a", "b", "c"].iter().enumerate() {
				k.entities
					.insert(id.to_string(), stamped(id, id, 100 + i as u64));
			}
			g.kerns.insert("kx".into(), k);
		}
		let g = srv.graph.read();
		let v = log_report(&g, None, 2).unwrap();
		let events = v["events"].as_array().unwrap();
		assert_eq!(events.len(), 2, "the cap bounds the answer");
		assert_eq!(events[0]["id"], "c", "newest first");
		assert_eq!(events[1]["id"], "b");
		assert_eq!(events[0]["change"], "added");
	}

	#[tokio::test]
	async fn a_thoughts_chain_carries_the_supersede_why() {
		let srv = crate::test_helpers::server();
		{
			let mut g = srv.graph.write();
			let mut k = Kern::new("kx", "");
			let old = {
				let mut e = stamped("old-rev", "the old claim", 100);
				let fell = std::time::UNIX_EPOCH + Duration::from_secs(200);
				e.status = base::base_types::EntityStatus::Superseded;
				e.stamp_invalidated(fell, fell);
				e
			};
			let new = stamped("new-rev", "the corrected claim", 200);
			k.entities.insert(old.id.clone(), old);
			k.entities.insert(new.id.clone(), new);
			// The Supersedes walk goes through the entity→kern index the real
			// insert paths maintain; a fixture that skips it never finds the chain.
			g.index_entity("old-rev", "kx");
			g.index_entity("new-rev", "kx");
			graph::reason::add_reason(
				&mut k,
				Reason {
					id: "sup".into(),
					from: "new-rev".into(),
					to: "old-rev".into(),
					kind: ReasonKind::Supersedes,
					text: "measurement contradicted it".into(),
					..Default::default()
				},
			);
			g.kerns.insert("kx".into(), k);
		}
		let g = srv.graph.read();
		let v = log_report(&g, Some("new-rev"), 0).unwrap();
		let revs = v["revisions"].as_array().unwrap();
		assert_eq!(revs.len(), 2, "head plus its superseded ancestor");
		assert_eq!(revs[0]["id"], "new-rev");
		assert_eq!(revs[0]["status"], "active");
		assert_eq!(revs[0]["why"], "", "the head needs no justification");
		assert_eq!(revs[1]["id"], "old-rev");
		assert_eq!(revs[1]["status"], "superseded");
		assert_eq!(
			revs[1]["why"], "measurement contradicted it",
			"the Supersedes edge text is the recorded why"
		);
		assert!(
			!revs[1]["invalidated"].as_str().unwrap().is_empty(),
			"a superseded revision carries when it fell"
		);
	}

	#[tokio::test]
	async fn an_unknown_id_is_an_error_not_an_empty_chain() {
		let srv = crate::test_helpers::server();
		let g = srv.graph.read();
		let err = log_report(&g, Some("nope"), 0).unwrap_err();
		assert!(err.contains("thought not found"), "{err}");
	}
}

// ==== events ====

use std::time::{SystemTime, UNIX_EPOCH};

// A default cap so a first poll of a large graph does not return every event
// ever recorded in one payload; the returned `cursor` resumes the rest.
const DEFAULT_LIMIT: usize = 100;

// created before superseded when two events on one entity ever share a nanosecond
// stamp (they cannot in practice — an entity is invalidated strictly after it is
// created — but the ordinal pins the tie-break so the cursor is a total order).
const CHANGE_CREATED: (&str, u8) = ("created", 0);
const CHANGE_SUPERSEDED: (&str, u8) = ("superseded", 1);

// The total order the feed is sorted by and the cursor resumes on. Field
// declaration order IS the comparison order (derived Ord): nanos, then entity
// id, then the change ordinal. Encoded to an opaque `nanos.ord.entity_id`
// string on the wire; compared as this tuple, never lexically, so the width of
// the numeric part is irrelevant to correctness.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct EventCursor {
	nanos: u128,
	entity_id: String,
	change_ord: u8,
}

impl EventCursor {
	fn encode(&self) -> String {
		format!("{}.{}.{}", self.nanos, self.change_ord, self.entity_id)
	}

	// `nanos.ord.entity_id`. The entity id is a content hash (no '.'), so a
	// 3-way split is unambiguous. Returns None on any malformed cursor so a
	// caller learns its cursor was rejected rather than silently rewinding.
	fn decode(s: &str) -> Option<Self> {
		let mut it = s.splitn(3, '.');
		let nanos: u128 = it.next()?.parse().ok()?;
		let change_ord: u8 = it.next()?.parse().ok()?;
		let entity_id = it.next()?.to_string();
		Some(EventCursor {
			nanos,
			entity_id,
			change_ord,
		})
	}
}

fn nanos_of(t: SystemTime) -> u128 {
	t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

// A single change, gathered under the read guard with its label projection so
// the render pass is a pure map over the sorted list — no second graph walk.
struct PendingEvent {
	cursor: EventCursor,
	change: &'static str,
	kind: &'static str,
	scheme: &'static str,
}

#[derive(Deserialize, Default)]
struct EventsArgs {
	// Untyped: a first call passes the integer 0, later calls pass back the
	// opaque string cursor. Both are accepted here and normalized below.
	#[serde(default)]
	since: serde_json::Value,
	#[serde(default)]
	limit: Option<usize>,
}

// The lower bound the feed resumes strictly after. `None` = from the beginning.
// A non-zero integer is treated as a bare nanosecond lower bound (ord 0, empty
// id), so `since: <nanos>` still means "after this instant".
fn parse_since(v: &serde_json::Value) -> Result<Option<EventCursor>, String> {
	match v {
		serde_json::Value::Null => Ok(None),
		serde_json::Value::Number(n) => {
			let raw = n.as_u64().ok_or_else(|| format!("invalid `since`: {n}"))?;
			if raw == 0 {
				Ok(None)
			} else {
				Ok(Some(EventCursor {
					nanos: raw as u128,
					entity_id: String::new(),
					change_ord: 0,
				}))
			}
		}
		serde_json::Value::String(s) => {
			if s.is_empty() || s == "0" {
				return Ok(None);
			}
			EventCursor::decode(s)
				.map(Some)
				.ok_or_else(|| format!("invalid `since` cursor: {s}"))
		}
		other => Err(format!("invalid `since`: {other}")),
	}
}

impl Server {
	pub(crate) fn tool_events(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: EventsArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};
		let since = parse_since(&p.since)?;
		let limit = match p.limit {
			Some(0) | None => DEFAULT_LIMIT,
			Some(n) => n,
		};

		// One read guard, no mutation: walk the resident kerns exactly as the
		// health/adjacency passes do and read the bitemporal stamps kern already
		// keeps. `created_at` dates the ingest; `invalidated_at` (set only on a
		// supersede, with status flipped) dates the revision going stale. Each
		// event carries its label projection so rendering needs no second walk.
		let mut events: Vec<PendingEvent> = Vec::new();
		{
			let g = self.graph.read();
			for kern in g.all() {
				for e in kern.entities.values() {
					let scheme = e.source.scheme();
					let kind = e.kind.as_str();
					if let Some(created) = e.created_at {
						events.push(PendingEvent {
							cursor: EventCursor {
								nanos: nanos_of(created),
								entity_id: e.id.clone(),
								change_ord: CHANGE_CREATED.1,
							},
							change: CHANGE_CREATED.0,
							kind,
							scheme,
						});
					}
					if e.is_superseded() {
						if let Some(inv) = e.invalidated_at {
							events.push(PendingEvent {
								cursor: EventCursor {
									nanos: nanos_of(inv),
									entity_id: e.id.clone(),
									change_ord: CHANGE_SUPERSEDED.1,
								},
								change: CHANGE_SUPERSEDED.0,
								kind,
								scheme,
							});
						}
					}
				}
			}
		}

		// Order by the cursor ascending, drop everything at or before `since`
		// (strictly greater = no overlap with the last delivered event), then cap.
		events.sort_by(|a, b| a.cursor.cmp(&b.cursor));
		let mut out: Vec<serde_json::Value> = Vec::new();
		let mut last: Option<EventCursor> = None;
		for ev in events {
			if let Some(ref lo) = since {
				if ev.cursor <= *lo {
					continue;
				}
			}
			if out.len() >= limit {
				break;
			}
			out.push(serde_json::json!({
				"entity_id": ev.cursor.entity_id,
				"kind": ev.kind,
				"change": ev.change,
				"at": ev.cursor.encode(),
				"source_scheme": ev.scheme,
			}));
			last = Some(ev.cursor);
		}

		// The next cursor: the last event we emitted, or — when nothing changed —
		// the caller's own position echoed back so a quiet poll never rewinds it.
		let cursor = last
			.map(|c| c.encode())
			.or_else(|| since.as_ref().map(EventCursor::encode))
			.unwrap_or_else(|| "0".to_string());

		Ok(serde_json::json!({
			"events": out,
			"cursor": cursor,
		}))
	}
}

// ==== mutate ====

use base::base_constants::AGENT_SOURCE;
use base::base_types::{Scoping, Source};
use graph::reason::move_entity;
use graph::search::find_entity;
use math::clamp_confidence;
use util::explain_relationship_prompt;
use util::validate_conf;

#[derive(Deserialize, Default)]
struct IngestArgs {
	#[serde(default)]
	text: String,
	#[serde(default)]
	source: String,
	#[serde(default)]
	object_id: String,
	#[serde(default)]
	section: String,
	#[serde(default)]
	author: String,
	#[serde(default)]
	title: String,
	#[serde(default)]
	url: String,
	#[serde(default)]
	conf: f64,
	#[serde(default)]
	hint: String,
	#[serde(default)]
	retention_secs: u64,
	#[serde(default)]
	sync: bool,
	#[serde(default)]
	user_id: Option<String>,
	#[serde(default)]
	agent_id: Option<String>,
	#[serde(default)]
	session_id: Option<String>,
}

// Caller boundary: an agent caller can mint neither Fact-kind nor Fact-confidence
// entities. Kind is derived from clamped confidence, never caller-supplied.
fn validate_ingest(p: &IngestArgs) -> Result<(), String> {
	validate_conf(p.conf).map_err(|e| e.to_string())?;
	Ok(())
}

#[derive(Deserialize)]
struct LinkArgs {
	from: String,
	to: String,
	#[serde(default)]
	reason: String,
}

#[derive(Deserialize)]
struct ForgetArgs {
	id: String,
}

#[derive(Deserialize)]
struct ForgetBySourceArgs {
	scheme: String,
	object_id: String,
	#[serde(default)]
	force: bool,
}

#[derive(Deserialize)]
struct DegradeArgs {
	query_id: String,
}

#[derive(Deserialize)]
struct MoveArgs {
	id: String,
	to_kern: String,
}

#[derive(Deserialize)]
struct PromoteArgs {
	id: String,
}

impl Server {
	pub(crate) fn tool_ingest(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: IngestArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};
		if p.text.is_empty() {
			return Err("text is required".to_string());
		}

		validate_ingest(&p)?;

		let valid_until = ingest::valid_until_from_retention(p.retention_secs)?;

		// Callers are agents; clamp against AGENT_SOURCE regardless of what
		// `p.source` claims — the caller's source string cannot escalate to USER_SOURCE trust.
		let (conf, kind) = clamp_confidence(p.conf, AGENT_SOURCE);
		let src = match p.source.as_str() {
			"" | "inline" => Source::Inline {
				hash: p.object_id,
				section: p.section,
			},
			"file" => Source::File {
				path: p.object_id,
				section: p.section,
				title: p.title,
				author: p.author,
				url: p.url,
			},
			"session" => Source::Session {
				session_id: p.object_id,
				section: p.section,
				title: p.title,
			},
			"agent" => Source::Agent {
				agent: p.source.clone(),
				object_id: p.object_id,
				title: p.title,
			},
			other => Source::Ticket {
				system: other.to_string(),
				object_id: p.object_id,
				section: p.section,
				title: p.title,
				author: p.author,
				url: p.url,
			},
		};

		let scoping = Scoping {
			user_id: p.user_id,
			agent_id: p.agent_id,
			session_id: p.session_id,
		};

		if p.sync {
			let fut = self.worker.run(
				p.text,
				src,
				kind,
				p.hint,
				conf,
				AGENT_SOURCE,
				ingest::Config {
					dedup_threshold: self.cfg.ingest.dedup_threshold,
					dedup_threshold_by_kind: self.cfg.ingest.dedup_threshold_by_kind,
					valid_until,
					review_policy: self.cfg.ingest.review_policy.clone(),
					hygiene: self.cfg.hygiene.gate_config(),
					..Default::default()
				},
				scoping.clone(),
			);
			let Some(outcome) = llm::block_on_in_place(fut) else {
				return Err("no tokio runtime".to_string());
			};
			(self.save_fn)();
			return Ok(serde_json::json!({
				"status": outcome.status.as_str(),
				"doc_id": outcome.doc_id,
				"conf": conf,
				"kind": kind as u8,
				"total_chunks": outcome.total_chunks,
				"embedded_chunks": outcome.embedded_chunks,
				"failed_chunks": outcome.failed_chunks,
				"transient_failures": outcome.transient_failures,
				"permanent_failures": outcome.permanent_failures,
				"message": outcome.message,
			}));
		}

		// Durable ack: persist to the direct intake BEFORE acknowledging, but only
		// when the drain loop runs — an undrained intake is worse than the RAM queue.
		let drain_runs = self.cfg.intake.enabled && !self.cfg.reason_url().is_empty();
		if drain_runs {
			let direct_dir = std::env::current_dir()
				.unwrap_or_else(|_| std::path::PathBuf::from("."))
				.join(&self.cfg.intake.dir)
				.join("direct");
			let job = ingest::direct::DirectJob {
				text: p.text.clone(),
				source: src.clone(),
				kind,
				hint: p.hint.clone(),
				confidence: conf,
				valid_until,
				valid_from: None,
				// Same principal the sync leg above clamps against: a caller is
				// an agent whatever `p.source` claims.
				source_tag: AGENT_SOURCE.to_string(),
				scoping: scoping.clone(),
			};
			match ingest::direct::intake_direct(&direct_dir, &job) {
				Ok(doc_id) => {
					return Ok(serde_json::json!({
						"status": "accepted",
						"doc_id": doc_id,
						"conf": conf,
						"kind": kind as u8,
					}));
				}
				Err(e) => {
					// Fail-open: an intake-write failure must not reject knowledge —
					// fall through to the RAM queue.
					tracing::warn!(
						target: "kern.ingest.direct",
						error = %e,
						"direct intake write failed; falling back to in-RAM enqueue"
					);
				}
			}
		}

		let Some(doc_id) = self.worker.enqueue(
			p.text,
			src,
			kind,
			p.hint,
			conf,
			AGENT_SOURCE,
			ingest::Config {
				dedup_threshold: self.cfg.ingest.dedup_threshold,
				dedup_threshold_by_kind: self.cfg.ingest.dedup_threshold_by_kind,
				valid_until,
				review_policy: self.cfg.ingest.review_policy.clone(),
				hygiene: self.cfg.hygiene.gate_config(),
				..Default::default()
			},
			scoping,
		) else {
			// Loud, not a `status` field in a success envelope: the caller has to
			// re-offer this text, and a caller that must act cannot be told quietly.
			return Err("ingest queue full; the text was not accepted, retry".to_string());
		};
		Ok(serde_json::json!({
			"status": "queued",
			"doc_id": doc_id,
			"conf": conf,
			"kind": kind as u8,
		}))
	}

	pub(crate) fn tool_link(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: LinkArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};

		let g = self.graph.read();
		let (from_t, _) = match find_entity(&g, &p.from) {
			Some(pair) => pair,
			None => return Err(format!("from thought not found: {}", p.from)),
		};
		let (to_t, _) = match find_entity(&g, &p.to) {
			Some(pair) => pair,
			None => return Err(format!("to thought not found: {}", p.to)),
		};
		drop(g);

		let mut reason_text = p.reason;
		if reason_text.is_empty() {
			if let Some(llm) = &self.llm {
				let prompt = explain_relationship_prompt(&from_t.text(), &to_t.text());
				if let Some(reply) = llm::block_on_in_place(llm.complete(&prompt)) {
					reason_text = reply.unwrap_or_default().trim().to_string();
				}
			}
		}

		let reason_embed = if !reason_text.is_empty() {
			self
				.llm
				.as_ref()
				.and_then(|llm| llm::block_on_in_place(llm.embed(&reason_text)))
				.and_then(Result::ok)
		} else {
			None
		};

		let mut g = self.graph.write();
		let res = graph::graph_ops::link_entities(
			&mut g,
			&p.from,
			&p.to,
			reason_text,
			reason_embed,
			base::base_constants::MAX_AI_CONFIDENCE,
		);
		drop(g);

		match res {
			Ok((rid, _)) => {
				(self.save_fn)();
				Ok(serde_json::json!({"edge_id": rid}))
			}
			Err(e) => Err(e),
		}
	}

	pub(crate) fn tool_forget(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: ForgetArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		let res = graph::graph_ops::forget_entity(&mut g, &p.id, false);
		drop(g);

		match res {
			Ok(removed) => {
				(self.save_fn)();
				Ok(serde_json::json!({"removed_edges": removed}))
			}
			Err("thought not found") => Err(format!("thought not found: {}", p.id)),
			Err(e) => Err(e.to_string()),
		}
	}

	// The routed half of `kern forget --source`. Exists so the command has
	// somewhere to route: a per-source forget with no operation behind it would
	// delete from the store behind a serving daemon's back.
	pub(crate) fn tool_forget_by_source(
		&self,
		args: &serde_json::Value,
	) -> Result<serde_json::Value, String> {
		let p: ForgetBySourceArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};
		let Some(scheme) = Source::parse_scheme(&p.scheme) else {
			return Err(format!("unknown source scheme: {}", p.scheme));
		};
		if p.object_id.is_empty() {
			return Err("object_id is required".to_string());
		}

		let mut g = self.graph.write();
		let out = graph::graph_ops::forget_by_source(&mut g, scheme, &p.object_id, p.force);
		drop(g);

		if out.removed_entities > 0 {
			(self.save_fn)();
		}
		Ok(serde_json::json!({
			"removed_entities": out.removed_entities,
			"removed_edges": out.removed_edges,
			"kept_facts": out.kept_facts,
		}))
	}

	pub(crate) fn tool_degrade(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: DegradeArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		let (_, kern_id) = match find_entity(&g, &p.query_id) {
			Some(pair) => pair,
			None => return Err(format!("thought not found: {}", p.query_id)),
		};

		let (decayed, removed) =
			graph::graph_ops::degrade_entity_reasons(&mut g, &kern_id, &p.query_id);
		drop(g);
		(self.save_fn)();

		Ok(serde_json::json!({
			"decayed_edges": decayed,
			"removed_edges": removed,
		}))
	}

	pub(crate) fn tool_move(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: MoveArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		let (_, from_kern_id) = match find_entity(&g, &p.id) {
			Some(pair) => pair,
			None => return Err(format!("thought not found: {}", p.id)),
		};

		// move_entity validates before it mutates, so a rejection here cannot have
		// left the graph half-moved — nothing to roll back, nothing to persist.
		if let Err(e) = move_entity(&mut g, &from_kern_id, &p.to_kern, &p.id) {
			return Err(e.to_string());
		}
		drop(g);
		(self.save_fn)();

		Ok(serde_json::json!({
			"id": p.id,
			"from_kern": from_kern_id,
			"to_kern": p.to_kern,
		}))
	}

	// The release half of the review lifecycle. Shares `promote_entity` with the
	// CLI's no-daemon fallback so the routed and local writes cannot disagree
	// about what "reviewed" means.
	pub(crate) fn tool_promote(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: PromoteArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		// An id nothing resolves is an error, never a quiet success: a caller
		// curating a typo would otherwise be told the row was released.
		let promoted = match graph::graph_ops::promote_entity(&mut g, &p.id) {
			Ok(v) => v,
			Err(e) => return Err(format!("{e}: {}", p.id)),
		};
		drop(g);
		// Nothing changed on a re-promote, so nothing to persist.
		if promoted {
			(self.save_fn)();
		}

		Ok(serde_json::json!({
			"id": p.id,
			"promoted": promoted,
		}))
	}
}

// ==== admin ====

#[derive(Deserialize, Default)]
struct GravitonArgs {
	#[serde(default)]
	action: String,
	#[serde(default)]
	name: String,
	#[serde(default)]
	text: String,
	#[serde(default)]
	mass: Option<f64>,
}

#[derive(Deserialize)]
struct ClaimKindArgs {
	action: String,
	name: String,
	#[serde(default)]
	description: String,
	#[serde(default)]
	parent: String,
}

#[derive(Deserialize, Default)]
struct PulseArgs {
	#[serde(default)]
	strength: f64,
}

impl Server {
	pub fn tool_health(&self) -> Result<serde_json::Value, String> {
		Ok(self.health_stats())
	}

	pub(crate) fn tool_graviton(
		&self,
		args: &serde_json::Value,
	) -> Result<serde_json::Value, String> {
		let p: GravitonArgs = serde_json::from_value(args.clone()).unwrap_or_default();
		let action = if p.action.is_empty() {
			"list"
		} else {
			p.action.as_str()
		};

		match action {
			"list" => {
				let g = self.graph.read();
				let gravitons: Vec<serde_json::Value> = graph::graph_ops::graviton_rows(&g)
					.into_iter()
					.map(|r| {
						serde_json::json!({
							"name": r.name,
							"mass": r.mass,
							"thoughts": r.thoughts,
							"reasons": r.reasons,
						})
					})
					.collect();
				Ok(serde_json::json!({ "gravitons": gravitons }))
			}
			"add" => {
				if p.name.is_empty() || p.text.is_empty() {
					return Err("add requires name and text".to_string());
				}
				// A multi-line seed is a list of example statements: each line is
				// embedded separately and mean-pooled, which places the graviton
				// ~0.16 cosine closer to real matching claims than embedding the
				// text whole (measured — see seed_examples).
				let examples = graph::accept::seed_examples(&p.text);
				let vec = match &self.llm {
					Some(llm) => {
						let mut vecs = Vec::with_capacity(examples.len());
						for ex in &examples {
							match llm::block_on_in_place(llm.embed(ex)) {
								Some(Ok(v)) => vecs.push(v),
								Some(Err(e)) => return Err(format!("embed failed: {e}")),
								None => return Err("no tokio runtime".to_string()),
							}
						}
						match graph::accept::mean_pool(&vecs) {
							Some(v) => v,
							None => return Err("empty or mismatched embeddings".to_string()),
						}
					}
					None => return Err("no embed client configured".to_string()),
				};
				let mut g = self.graph.write();
				graph::accept::add_graviton_with_mass(&mut g, &p.name, vec, p.mass.unwrap_or(1.0));
				drop(g);
				(self.save_fn)();
				Ok(serde_json::json!({ "added": p.name }))
			}
			"remove" | "rm" => {
				if p.name.is_empty() {
					return Err("remove requires name".to_string());
				}
				let mut g = self.graph.write();
				let removed = graph::accept::remove_graviton(&mut g, &p.name);
				drop(g);
				if removed {
					(self.save_fn)();
					Ok(serde_json::json!({ "removed": p.name }))
				} else {
					Err(format!("graviton not found: {}", p.name))
				}
			}
			_ => Err("action must be add, list, or remove".to_string()),
		}
	}

	pub(crate) fn tool_claim_kind(
		&self,
		args: &serde_json::Value,
	) -> Result<serde_json::Value, String> {
		let p: ClaimKindArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return Err(format!("invalid arguments: {e}")),
		};

		match p.action.as_str() {
			"add" => {
				if p.description.is_empty() {
					return Err("description required for add".to_string());
				}
				let parent = (!p.parent.is_empty()).then_some(p.parent.as_str());
				let mut g = self.graph.write();
				g.root.add_claim_kind(
					&p.name,
					&p.description,
					parent,
					&ingest::distill::DEFAULT_KINDS,
				)?;
				drop(g);
				(self.save_fn)();
				Ok(serde_json::json!({"added": p.name}))
			}
			"rm" => {
				let mut g = self.graph.write();
				g.root.rm_claim_kind(&p.name);
				drop(g);
				(self.save_fn)();
				Ok(serde_json::json!({"removed": p.name}))
			}
			_ => Err("action must be add or rm".to_string()),
		}
	}

	// Write lock held only for the reap; no env close, so safe while serving.
	pub(crate) fn tool_audit(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let min_score = args
			.get("min_score")
			.and_then(|v| v.as_f64())
			.unwrap_or(0.3);
		if !(0.0..=1.0).contains(&min_score) {
			return Err("min_score must be in [0.0, 1.0]".to_string());
		}
		let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
		let apply = match args.get("apply").and_then(|v| v.as_str()) {
			None | Some("") => None,
			Some(s) => match graph::graph_ops::AuditAction::parse(s) {
				Some(a) => Some(a),
				None => return Err("apply takes archive or delete".to_string()),
			},
		};
		let report = {
			let g = self.graph.read();
			graph::graph_ops::audit_noise(&g, min_score, limit)
		};
		let Some(action) = apply else {
			return Ok(serde_json::json!({
				"scanned": report.scanned,
				"candidates": report.candidates,
				"note": "report only — pass apply: archive (reversible) or delete to act",
			}));
		};
		let out = {
			let mut g = self.graph.write();
			graph::graph_ops::apply_audit(&mut g, min_score, action)
		};
		if out.archived + out.deleted > 0 {
			(self.save_fn)();
		}
		Ok(serde_json::json!({
			"scanned": report.scanned,
			"candidates": report.candidates,
			"applied": out,
		}))
	}

	pub(crate) fn tool_gc(&self) -> Result<serde_json::Value, String> {
		let (before, reaped, after) = {
			let mut g = self.graph.write();
			g.gc_empty_kerns_counted()
		};
		if reaped > 0 {
			(self.save_fn)();
		}
		// LMDB keeps freed pages until a restart/`kern compact`.
		let data_bytes = self
			.graph
			.read()
			.store()
			.map(|s| s.data_file_len())
			.unwrap_or(0);
		Ok(serde_json::json!({
			"reaped": reaped,
			"before": before,
			"after": after,
			"data_mdb_bytes": data_bytes,
			"note": if data_bytes > 256 * 1024 * 1024 {
				"rows pruned live; data.mdb keeps freed pages until the next restart auto-compacts (or run `kern compact` with the daemon stopped)"
			} else {
				"clean"
			},
		}))
	}

	pub(crate) fn tool_pulse(&self, args: &serde_json::Value) -> Result<serde_json::Value, String> {
		let p: PulseArgs = serde_json::from_value(args.clone()).unwrap_or_default();
		let strength = if p.strength <= 0.0 { 1.0 } else { p.strength };

		let q = match &self.task_q {
			None => {
				return Ok(serde_json::json!({
					"status": "noop",
					"enqueued": 0,
					"reason": "no task queue configured; pulse requires the daemon tick queue",
				}))
			}
			Some(q) => q,
		};

		let g = self.graph.read();
		let root_id = g.root.id.clone();
		tick::tick_pulse::pulse(q, &g, &root_id, strength);
		drop(g);

		if let Some(broadcast) = &self.broadcast_pulse {
			broadcast(&root_id, strength);
		}

		Ok(serde_json::json!({"status": "pulsed", "strength": strength}))
	}
}

use std::time::Duration;

impl Server {
	// Runs the same pass the daemon's poll loop runs, in the daemon. A caller
	// draining in its own process reads the same directory and archives the same
	// entries, so both distill the file and both race the archive move.
	pub(crate) fn tool_intake_drain(&self) -> Result<serde_json::Value, String> {
		let dir = std::env::current_dir()
			.unwrap_or_else(|_| std::path::PathBuf::from("."))
			.join(&self.cfg.intake.dir);
		let llm_fn: Option<ingest::LlmFunc> = match &self.llm {
			Some(c) if c.has_reason() => Some(std::sync::Arc::new(c.complete_func())),
			_ => None,
		};
		let extra_kinds: Vec<String> = self.graph.read().root.claim_kinds.keys().cloned().collect();

		let archived = llm::block_on_in_place(ingest::intake::drain_now(
			&dir,
			&self.worker,
			llm_fn.as_ref(),
			&extra_kinds,
			self.cfg.ingest.dedup_threshold,
			self.cfg.intake.retention_secs,
			self.cfg.ingest.review_policy.clone(),
			self.cfg.hygiene.gate_config(),
			Duration::from_secs(self.cfg.intake.done_retention_secs),
			SystemTime::now(),
		));
		let Some(archived) = archived else {
			return Err("no tokio runtime".to_string());
		};
		if archived > 0 {
			(self.save_fn)();
		}
		Ok(serde_json::json!({"archived": archived}))
	}
}

// ==== setup ====

// The agent-facing installer. kern never writes into a host's config itself —
// the host layout is the agent's domain — so `setup` returns instructions and
// the current gaps, and the calling agent does the wiring. One instruction set
// instead of one plugin per host.

pub(crate) struct SetupState {
	pub gravitons: Vec<String>,
	pub thoughts: u64,
	pub claim_kinds: u64,
	pub intake_dir: String,
}

fn check(done: bool) -> &'static str {
	if done {
		"[done]"
	} else {
		"[todo]"
	}
}

pub(crate) fn render_setup(s: &SetupState) -> String {
	let mut out = String::new();
	out.push_str(
		"# kern setup — instructions for the calling agent\n\
		\n\
		kern is this project's persistent memory: a per-directory daemon holding a\n\
		knowledge graph. You (the agent) wire it into your host by following the\n\
		steps below. Every step is idempotent — skip anything already [done].\n\n",
	);

	out.push_str("## Current state\n\n");
	out.push_str(&format!(
		"- {} gravitons seeded ({})\n",
		check(!s.gravitons.is_empty()),
		if s.gravitons.is_empty() {
			"none".to_string()
		} else {
			s.gravitons.join(", ")
		}
	));
	out.push_str(&format!(
		"- {} claim kinds registered ({})\n",
		check(s.claim_kinds > 0),
		s.claim_kinds
	));
	out.push_str(&format!(
		"- {} memory has content ({} thoughts)\n\n",
		check(s.thoughts > 0),
		s.thoughts
	));

	if s.gravitons.is_empty() {
		out.push_str(
			"## Seed gravitons (do this first)\n\
			\n\
			Gravitons are the focus areas ingest routes into. Call the `graviton`\n\
			operation once per area; 3-6 is right. Seed `text` with EXAMPLE STATEMENTS,\n\
			one per line, shaped like the claims that should land there — each line\n\
			is embedded separately and mean-pooled, which routes measurably better\n\
			than an abstract description. Write examples about THIS project:\n\
			\n\
			- {\"action\":\"add\",\"name\":\"decisions\",\"text\":\"We chose X over Y because <reason>.\\nWe rejected Z; <reason>.\\nDecision: <choice>, with the rationale recorded.\"}\n\
			- {\"action\":\"add\",\"name\":\"architecture\",\"text\":\"The <component> owns <responsibility>.\\nData flows from <a> to <b> via <mechanism>.\\nPersistence is <storage layer>.\"}\n\
			- {\"action\":\"add\",\"name\":\"preferences\",\"text\":\"Code style: <rule>.\\nAlways <practice> before <event>.\\nAvoid <antipattern>.\"}\n\n",
		);
	}

	out.push_str(
		"## Wire capture into your host\n\
		\n\
		kern captures nothing on its own — you feed it. Two entry points:\n\
		\n\
		- `kern ingest` (preferred): store one durable claim directly.\n\
		- `.kern/intake/` drop dir: write a session transcript as a `.txt` file\n\
		  there and the daemon distills it into claims in the background.\n\
		\n\
		Install a standing rule in your host's instruction file — CLAUDE.md,\n\
		AGENTS.md, .cursorrules, or the equivalent your host reads. Detect which\n\
		applies by looking at the project root and your own configuration, then\n\
		add this block if it is not already present:\n\
		\n\
		```\n\
		## Memory (kern)\n\
		- At task start: run `kern query` with the task topic to recall prior\n\
		  decisions, preferences, and facts before deciding anything.\n\
		- At task end, and whenever a durable decision, preference, constraint,\n\
		  or hard-won fact emerges: run `kern ingest` with ONE self-contained\n\
		  statement per fact. Include the why on decisions.\n\
		- When recall returns something wrong or stale: run `kern degrade` with\n\
		  the query id so it stops surfacing.\n\
		```\n\
		\n\
		If your host supports lifecycle hooks (e.g. a session-end hook), also\n\
		wire one that appends the session transcript to `",
	);
	out.push_str(&s.intake_dir);
	out.push_str(
		"/<timestamp>.txt`\n\
		so nothing depends on you remembering to ingest.\n\n",
	);

	out.push_str(
		"## Verify\n\
		\n\
		1. Run `kern ingest` with {\"text\":\"kern setup verified for this project.\",\"sync\":true} — expect status committed.\n\
		2. Run `kern health` — expect `thoughts` to have increased.\n\
		3. Run `kern query` with {\"text\":\"kern setup\"} — expect the claim back.\n\
		\n\
		If ingest fails: the embedding endpoint is down or misconfigured — check\n\
		`.kern/kern.toml` [embed] and see the `health` embed_mismatch flag.\n\n",
	);

	out.push_str(
		"## Tune (optional)\n\
		\n\
		Memory tuning is one line in `.kern/kern.toml` — the preset owns every\n\
		heat/dedup/retrieval knob; there are no individual keys to set:\n\
		\n\
		- `preset = \"relaxed\"` — the default: keep more, deliver more, forget slower\n\
		- `preset = \"medium\"` — balanced\n\
		- `preset = \"tight\"` — aggressive dedup, faster decay, fewer but sharper results\n\n",
	);

	out.push_str(
		"## Ongoing contract\n\
		\n\
		Query before deciding, ingest after deciding, degrade what misleads.\n\
		Claims should be atomic, standalone statements — not summaries of a\n\
		whole session. kern dedupes aggressively; re-ingesting a known fact is\n\
		cheap and reinforces it.\n",
	);

	out
}

impl Server {
	pub(crate) fn tool_setup(&self) -> Result<serde_json::Value, String> {
		let (gravitons, thoughts, claim_kinds) = {
			let g = self.graph.read();
			let h = ::health::graph_health_stats(&g);
			(
				h.gravitons,
				h.entities as u64,
				g.root.claim_kinds.len() as u64,
			)
		};
		let intake_dir = self.cfg.intake.dir.clone();
		let state = SetupState {
			gravitons,
			thoughts,
			claim_kinds,
			intake_dir,
		};
		Ok(serde_json::json!({ "instructions": render_setup(&state) }))
	}
}

#[cfg(test)]
#[path = "tests/server_query_test.rs"]
mod server_query_tests;

#[cfg(test)]
#[path = "tests/server_mutate_test.rs"]
mod server_mutate_tests;

#[cfg(test)]
#[path = "tests/server_admin_test.rs"]
mod server_admin_tests;

#[cfg(test)]
#[path = "tests/server_events_test.rs"]
mod server_events_tests;

#[cfg(test)]
#[path = "tests/server_setup_test.rs"]
mod server_setup_tests;
