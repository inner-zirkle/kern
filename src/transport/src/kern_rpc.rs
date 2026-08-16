//! The kern RPC: the tolerant-decode DTOs (a CLI upgraded ahead of a
//! long-running detached daemon must still be able to talk to it), the
//! `service!`-generated client/server pair, and the local attach client. The
//! socket lives in the daemon's own data dir, so filesystem ownership is the
//! access model — there is no token handshake.

use serde::{Deserialize, Serialize};

use crate::typed::{AdapterError, Channel, JsonEnvelopeCodec};

#[cfg(test)]
#[path = "tests/kern_rpc_test.rs"]
mod kern_rpc_tests;

use std::time::Duration;

use crate::typed::{connect_kern, Endpoint};

pub const RETRIES: u32 = 5;
pub const RETRY_DELAY_MS: u64 = 100;

impl KernRpcClient<JsonEnvelopeCodec> {
	pub async fn connect_local() -> Result<Self, AdapterError> {
		Self::connect_endpoint(&Endpoint::kern()).await
	}

	pub async fn connect_endpoint(endpoint: &Endpoint) -> Result<Self, AdapterError> {
		Self::connect_endpoint_with_retry(endpoint, RETRIES, Duration::from_millis(RETRY_DELAY_MS))
			.await
	}

	pub async fn connect_endpoint_with_retry(
		endpoint: &Endpoint,
		retries: u32,
		base_delay: Duration,
	) -> Result<Self, AdapterError> {
		let mut last_err: Option<AdapterError> = None;
		for _ in 0..retries {
			match connect_kern(endpoint).await {
				Ok(adapter) => {
					let channel = Channel::new(adapter, JsonEnvelopeCodec::new());
					return Ok(KernRpcClient::new(channel));
				}
				// Propagated, never retried: the endpoint is bound by something this
				// user does not own. Waiting cannot make it ours, and the retry loop
				// exists for a daemon that has not finished starting, not for one
				// that is not there at all.
				Err(e @ AdapterError::UntrustedEndpoint(_)) => return Err(e),
				Err(e) => last_err = Some(e),
			}
			tokio::time::sleep(jittered(base_delay)).await;
		}
		Err(last_err.unwrap_or_else(|| AdapterError::Other("no endpoint".into())))
	}
}

fn jittered(base: Duration) -> Duration {
	let base_ms = base.as_millis() as u64;
	if base_ms == 0 {
		return base;
	}
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.subsec_nanos() as u64)
		.unwrap_or(0);
	let half = base_ms / 2;
	Duration::from_millis(half + (nanos % (half + 1)))
}

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeWeightsHealth {
	#[serde(default)]
	pub content: f64,
	#[serde(default)]
	pub reason: f64,
	#[serde(default)]
	pub edge: f64,
}

// Active RRF config (`RetrievalConfig.rrf_k` / `rrf_global_weight` / the three
// `ModeWeights`) plus the remaining active retrieval knobs (`seed_k`,
// `mmr_enabled`, `lexical_enabled`, `pagerank_enabled`), preset-owned. Zeroed
// from older daemons (ROADMAP item 66 measurement half).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RetrievalHealth {
	#[serde(default)]
	pub rrf_k: f64,
	#[serde(default)]
	pub rrf_global_weight: f64,
	#[serde(default)]
	pub weights_content: ModeWeightsHealth,
	#[serde(default)]
	pub weights_reason: ModeWeightsHealth,
	#[serde(default)]
	pub weights_hybrid: ModeWeightsHealth,
	#[serde(default)]
	pub seed_k: usize,
	#[serde(default)]
	pub mmr_enabled: bool,
	#[serde(default)]
	pub lexical_enabled: bool,
	#[serde(default)]
	pub pagerank_enabled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ShutdownRes {
	pub ok: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HealthRes {
	pub ok: bool,
	#[serde(default)]
	pub data_dir: String,
	#[serde(default)]
	pub kerns: u64,
	#[serde(default)]
	pub entities: u64,
	// Ms since the last real tool call (health polls excluded). 0 from older
	// daemons that predate the field — the hub treats that as "never idle".
	#[serde(default)]
	pub idle_ms: u64,
	#[serde(default)]
	pub queue_depth: u64,
	#[serde(default)]
	pub tasks_done: u64,
	// Lifetime mean over `tasks_done`, not a recent window: it converges and
	// stops moving, so read it as a baseline, never as current load.
	#[serde(default)]
	pub task_avg_ms: u64,
	// Degraded maintenance. A panic killed its task; a failure ended it early and
	// re-enqueues forever. Empty string = none recorded, including on old daemons.
	#[serde(default)]
	pub task_panics: u64,
	#[serde(default)]
	pub last_task_panic: String,
	#[serde(default)]
	pub task_failures: u64,
	#[serde(default)]
	pub last_task_failure: String,
	// Store health: cold rows the FIFO cap dropped, and the embedding stamp the
	// index was built with. `embed_mismatch` means the live model is not that one.
	#[serde(default)]
	pub cold_evicted: u64,
	#[serde(default)]
	pub embed_model: String,
	#[serde(default)]
	pub embed_dim: u64,
	#[serde(default)]
	pub embed_mismatch: bool,
	// Fail-open degradations. Each is a path that returns something rather than
	// erroring, so the count is the only way to tell a degraded result from a
	// good one: queries the dimension guard dropped, deliveries that bypassed
	// `min_deliver_score` because nothing cleared it, and entities GC could not
	// age because their timestamp is in the future.
	#[serde(default)]
	pub query_dim_rejected: u64,
	#[serde(default)]
	pub below_floor_deliveries: u64,
	#[serde(default)]
	pub clock_skew_skips: u64,
	#[serde(default)]
	pub ingest_dropped_chunks: u64,
	#[serde(default)]
	pub unspilled_drops: u64,
	#[serde(default)]
	pub ingest_queue_refused: u64,
	// Jobs parked in the ingest RAM queue right now — a gauge, not a counter.
	#[serde(default)]
	pub ingest_queue_depth: u64,
	// Gini over resident entities' access counts: 0.0 = uniform (converged),
	// →1.0 = one entity holds all access. 0.0 from older daemons (item 62).
	#[serde(default)]
	pub gini_access: f64,
	// The resident-kern cap: 0 = old daemon / unset, `u64::MAX` = uncapped
	// (`KERN_CAP_DISABLED`). A live bound is >= 1 (item 83).
	#[serde(default)]
	pub max_kerns: u64,
	// Propagations the trainer refused past its queue cap. Those kerns keep the
	// `gnn_vector` they already had, so the count is the only trace.
	#[serde(default)]
	pub gnn_train_refused: u64,
	// Supersede chains that exceeded `SUPERSEDE_CHAIN_HOP_THRESHOLD` on one
	// `external_id` (ROADMAP item 58 trigger #1). 0 from older daemons.
	#[serde(default)]
	pub supersede_chain_depth_exceeded: u64,
	// The largest resident kern's entity count (ROADMAP item 83). 0 from older
	// daemons.
	#[serde(default)]
	pub largest_kern_entities: usize,
	// Gini over resident kern sizes (ROADMAP item 83). 0.0 from older daemons.
	#[serde(default)]
	pub gini_kern_sizes: f64,
	// Active heat retention half-life (`HeatConfig.half_life_secs`, the one
	// `Preset::apply` sets — relaxed=30d / medium=7d / tight=3d, never a config
	// edit). 0 from older daemons (ROADMAP item 62 `kern://health` surfacing).
	#[serde(default)]
	pub heat_half_life_secs: u64,
	// QBST recency half-life (`RetrievalConfig.qbst_recency_half_life_secs`,
	// the 24h ranking-freshness signal). 0 from older daemons (ROADMAP item 55).
	#[serde(default)]
	pub qbst_recency_half_life_secs: u64,
	// Active RRF config + mode blends (ROADMAP item 66 measurement half).
	// Zeroed from older daemons.
	#[serde(default)]
	pub retrieval: RetrievalHealth,
	// Active preset name (`Config.preset`, `Preset::apply` is its only writer).
	// Empty from older daemons (ROADMAP item 87 measurement half).
	#[serde(default)]
	pub preset: String,
	// Active source-trust map (`RetrievalConfig.source_trust`, keyed on
	// `Source::scheme()` — file/ticket/session/agent/inline). Empty from
	// older daemons and from a configless kern (ROADMAP item 20 measurement
	// half).
	#[serde(default)]
	pub source_trust: BTreeMap<String, f64>,
	// Active ingest dedup config (`IngestConfig.dedup_threshold` + the
	// per-kind `dedup_threshold_by_kind` array, shipped 2026-07-23 by item 48
	// beside). `0.0` / `[None; 5]` from older daemons and from a configless
	// kern (ROADMAP item 48 measurement half). The array is indexed by
	// `EntityKind as u8` (Fact=0 .. Conclusion=4); `None` falls back to the
	// global threshold.
	#[serde(default)]
	pub ingest_dedup_threshold: f64,
	#[serde(default)]
	pub ingest_dedup_threshold_by_kind: [Option<f64>; 5],
	// Completions that failed on the reason endpoint, and the last one in words.
	// The blocking bridge hands its caller `""` for every failure, so the count
	// is what separates a dead endpoint from a model with nothing to say, and the
	// string is what separates a timeout from a refusal from an empty body.
	#[serde(default)]
	pub llm_complete_failed: u64,
	#[serde(default)]
	pub last_llm_complete_failure: String,
	// Staleness identity. `build_id` fingerprints the running executable,
	// `config_id` the resolved config, so an edited kern.toml reads as stale
	// even when the binary did not move. Empty from daemons predating the
	// fields — and empty must never be treated as a mismatch, or every attach
	// to an older daemon would restart it.
	#[serde(default)]
	pub build_id: String,
	#[serde(default)]
	pub config_id: String,
	// Ms since the daemon booted. Guards the auto-restart against thrash when
	// two clients with different builds alternate. 0 = unknown, do not restart.
	#[serde(default)]
	pub uptime_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InvokeReq {
	pub name: String,
	#[serde(default)]
	pub args: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InvokeRes {
	#[serde(default)]
	pub value: serde_json::Value,
	// Empty = ok. A non-empty error carries the refusal text.
	#[serde(default)]
	pub error: String,
}

crate::service! {
		pub trait KernRpc {
				async fn health() -> HealthRes;
				async fn shutdown() -> ShutdownRes;
				async fn invoke(req: InvokeReq) -> InvokeRes;
		}
}
