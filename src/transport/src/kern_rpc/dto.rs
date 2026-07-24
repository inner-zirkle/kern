use serde::{Deserialize, Serialize};
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
	pub remote_cap_dropped: u64,
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
pub struct CallToolReq {
	pub name: String,
	#[serde(default)]
	pub args: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallToolRes {
	pub envelope: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ListToolsReq {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ListToolsRes {
	pub tools: Vec<serde_json::Value>,
}

#[cfg(test)]
mod dto_serde_tests {
	use super::*;

	#[test]
	fn an_older_health_payload_without_queue_fields_still_deserializes() {
		let old = r#"{"ok":true,"data_dir":"/d","kerns":3,"entities":7,"idle_ms":42}"#;
		let h: HealthRes = serde_json::from_str(old).expect("append-only: old shape must decode");
		assert_eq!(h.kerns, 3);
		assert_eq!(h.idle_ms, 42);
		assert_eq!(h.queue_depth, 0, "absent field defaults, never errors");
		assert_eq!(h.tasks_done, 0);
		assert_eq!(h.task_avg_ms, 0);
		assert_eq!(h.task_panics, 0);
		assert!(h.last_task_panic.is_empty());
		assert_eq!(h.task_failures, 0);
		assert!(h.last_task_failure.is_empty());
		assert_eq!(h.cold_evicted, 0);
		assert!(h.embed_model.is_empty());
		assert_eq!(h.embed_dim, 0);
		assert!(!h.embed_mismatch, "an old daemon is not a mismatching one");
		assert_eq!(h.query_dim_rejected, 0);
		assert_eq!(h.below_floor_deliveries, 0);
		assert_eq!(
			h.clock_skew_skips, 0,
			"an old daemon reports no degradation"
		);
		assert_eq!(h.ingest_dropped_chunks, 0);
		assert_eq!(h.remote_cap_dropped, 0);
		assert_eq!(h.unspilled_drops, 0);
		assert_eq!(h.ingest_queue_refused, 0);
		assert_eq!(h.ingest_queue_depth, 0);
		assert_eq!(h.gnn_train_refused, 0);
		assert_eq!(h.llm_complete_failed, 0);
		assert!(h.last_llm_complete_failure.is_empty());
		assert!(h.build_id.is_empty(), "unknown build, not a stale one");
		assert!(h.config_id.is_empty());
		assert_eq!(h.uptime_ms, 0);
		assert_eq!(h.largest_kern_entities, 0);
		assert!((h.gini_kern_sizes - 0.0).abs() < 1e-12);
		assert!((h.retrieval.rrf_k - 0.0).abs() < 1e-12);
		assert!((h.retrieval.rrf_global_weight - 0.0).abs() < 1e-12);
		assert!((h.retrieval.weights_content.content - 0.0).abs() < 1e-12);
		assert_eq!(h.retrieval.seed_k, 0);
		assert!(!h.retrieval.mmr_enabled);
		assert!(!h.retrieval.lexical_enabled);
		assert!(!h.retrieval.pagerank_enabled);
		assert!(h.preset.is_empty(), "an old daemon reports no preset name");
		assert!(
			h.source_trust.is_empty(),
			"an old daemon reports no source-trust map"
		);
		assert!(
			(h.ingest_dedup_threshold - 0.0).abs() < 1e-12,
			"an old daemon reports no ingest dedup threshold"
		);
		assert!(
			h.ingest_dedup_threshold_by_kind.iter().all(Option::is_none),
			"an old daemon reports no per-kind dedup overrides"
		);

		let ancient = r#"{"ok":true}"#;
		let h2: HealthRes = serde_json::from_str(ancient).expect("only `ok` is required");
		assert!(h2.ok);
		assert_eq!(h2.task_avg_ms, 0);
	}

	#[test]
	fn every_health_field_round_trips_through_json() {
		let src = HealthRes {
			ok: true,
			data_dir: "/d".into(),
			kerns: 1,
			entities: 2,
			idle_ms: 3,
			queue_depth: 4,
			tasks_done: 5,
			task_avg_ms: 6,
			task_panics: 7,
			last_task_panic: "GnnPropagate[k]: boom".into(),
			task_failures: 8,
			last_task_failure: "GnnPropagate[k]: train epoch 0 forward".into(),
			cold_evicted: 9,
			embed_model: "qwen3".into(),
			embed_dim: 1024,
			embed_mismatch: true,
			query_dim_rejected: 11,
			below_floor_deliveries: 12,
			clock_skew_skips: 13,
			ingest_dropped_chunks: 14,
			remote_cap_dropped: 15,
			unspilled_drops: 16,
			ingest_queue_refused: 17,
			ingest_queue_depth: 21,
			gini_access: 0.42,
			max_kerns: 128,
			gnn_train_refused: 18,
			supersede_chain_depth_exceeded: 22,
			largest_kern_entities: 99,
			gini_kern_sizes: 0.42,
			heat_half_life_secs: 2592000,
			qbst_recency_half_life_secs: 86400,
			retrieval: RetrievalHealth {
				rrf_k: 60.0,
				rrf_global_weight: 0.5,
				weights_content: ModeWeightsHealth {
					content: 0.7,
					reason: 0.2,
					edge: 0.1,
				},
				weights_reason: ModeWeightsHealth {
					content: 0.1,
					reason: 0.8,
					edge: 0.1,
				},
				weights_hybrid: ModeWeightsHealth {
					content: 0.5,
					reason: 0.3,
					edge: 0.2,
				},
				seed_k: 30,
				mmr_enabled: false,
				lexical_enabled: true,
				pagerank_enabled: true,
			},
			preset: "tight".into(),
			source_trust: BTreeMap::from([
				("file".to_string(), 0.8),
				("ticket".to_string(), 0.9),
			]),
			ingest_dedup_threshold: 0.95,
			ingest_dedup_threshold_by_kind: [Some(0.99), None, None, None, None],
			llm_complete_failed: 19,
			last_llm_complete_failure: "transient: HTTP error: operation timed out".into(),
			build_id: "a1b2c3d4e5f60718".into(),
			config_id: "0f1e2d3c4b5a6978".into(),
			uptime_ms: 90_000,
		};
		let back: HealthRes = serde_json::from_str(&serde_json::to_string(&src).unwrap()).unwrap();
		assert_eq!(back.task_panics, 7);
		assert_eq!(back.last_task_panic, src.last_task_panic);
		assert_eq!(back.task_failures, 8);
		assert_eq!(back.last_task_failure, src.last_task_failure);
		assert_eq!(back.cold_evicted, 9);
		assert_eq!(back.embed_model, "qwen3");
		assert_eq!(back.embed_dim, 1024);
		assert!(back.embed_mismatch);
		assert_eq!(back.query_dim_rejected, 11);
		assert_eq!(back.below_floor_deliveries, 12);
		assert_eq!(back.clock_skew_skips, 13);
		assert_eq!(back.ingest_dropped_chunks, 14);
		assert_eq!(back.remote_cap_dropped, 15);
		assert_eq!(back.unspilled_drops, 16);
		assert_eq!(back.ingest_queue_refused, 17);
		assert_eq!(back.ingest_queue_depth, 21);
		assert!((back.gini_access - 0.42).abs() < 1e-12);
		assert_eq!(back.max_kerns, 128);
		assert_eq!(back.gnn_train_refused, 18);
		assert_eq!(back.supersede_chain_depth_exceeded, 22);
		assert_eq!(back.largest_kern_entities, 99);
		assert!((back.gini_kern_sizes - 0.42).abs() < 1e-12);
		assert_eq!(back.heat_half_life_secs, 2592000);
		assert_eq!(back.qbst_recency_half_life_secs, 86400);
		assert_eq!(back.retrieval.rrf_k, 60.0);
		assert!((back.retrieval.rrf_global_weight - 0.5).abs() < 1e-12);
		assert!((back.retrieval.weights_content.content - 0.7).abs() < 1e-12);
		assert!((back.retrieval.weights_reason.reason - 0.8).abs() < 1e-12);
		assert!((back.retrieval.weights_hybrid.edge - 0.2).abs() < 1e-12);
	assert_eq!(back.retrieval.seed_k, 30);
	assert!(!back.retrieval.mmr_enabled);
	assert!(back.retrieval.lexical_enabled);
	assert!(back.retrieval.pagerank_enabled);
		assert_eq!(back.preset, "tight");
		assert_eq!(
			back.source_trust.get("file").copied().unwrap_or(0.0),
			0.8
		);
		assert_eq!(
			back.source_trust.get("ticket").copied().unwrap_or(0.0),
			0.9
		);
		assert!((back.ingest_dedup_threshold - 0.95).abs() < 1e-12);
		assert_eq!(
			back.ingest_dedup_threshold_by_kind,
			[Some(0.99), None, None, None, None]
		);
		assert_eq!(back.llm_complete_failed, 19);
		assert_eq!(
			back.last_llm_complete_failure,
			src.last_llm_complete_failure
		);
		assert_eq!(back.build_id, src.build_id);
		assert_eq!(back.config_id, src.config_id);
		assert_eq!(back.uptime_ms, 90_000);
	}
}
