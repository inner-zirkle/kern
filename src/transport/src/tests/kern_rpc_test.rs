//! Tests extracted from kern_rpc.rs
#![allow(unused)]
use super::*;

mod client_tests {
	use super::*;

	fn bogus_endpoint() -> Endpoint {
		#[cfg(unix)]
		{
			Endpoint::Unix(std::path::PathBuf::from(
				"/nonexistent/kern-test-bogus.sock",
			))
		}
		#[cfg(windows)]
		{
			Endpoint::NamedPipe(r"\\.\pipe\kern-test-bogus-nonexistent".to_string())
		}
	}

	#[test]
	fn jittered_stays_within_half_to_full_and_zero_stays_zero() {
		assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
		for _ in 0..64 {
			let d = jittered(Duration::from_millis(100));
			assert!(
				d >= Duration::from_millis(50) && d <= Duration::from_millis(100),
				"jitter must stay in [base/2, base], got {d:?}",
			);
		}
	}

	#[tokio::test]
	async fn connect_endpoint_gives_up_after_exhausting_retries() {
		let res =
			KernRpcClient::connect_endpoint_with_retry(&bogus_endpoint(), 3, Duration::from_millis(1))
				.await;
		assert!(
			res.is_err(),
			"no server at the endpoint -> Err after retries"
		);
	}

	// A socket somebody else owns is refused by the owner check, which sits
	// ahead of `connect`. Skipped under an euid of 0, where nothing on the
	// filesystem is foreign and the case cannot fail.
	#[cfg(unix)]
	#[tokio::test]
	async fn a_foreign_owned_endpoint_is_refused() {
		// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
		if unsafe { libc::geteuid() } == 0 || !std::path::Path::new("/etc/hosts").exists() {
			return;
		}
		let err = KernRpcClient::connect_endpoint_with_retry(
			&Endpoint::Unix(std::path::PathBuf::from("/etc/hosts")),
			3,
			Duration::from_millis(1),
		)
		.await
		.err()
		.expect("a foreign endpoint never yields a client");
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"refused by the owner check, before any frame: {err}"
		);
	}
}
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
			source_trust: BTreeMap::from([("file".to_string(), 0.8), ("ticket".to_string(), 0.9)]),
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
		assert_eq!(back.source_trust.get("file").copied().unwrap_or(0.0), 0.8);
		assert_eq!(back.source_trust.get("ticket").copied().unwrap_or(0.0), 0.9);
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

	#[test]
	fn invoke_res_round_trips_ok_and_error() {
		let ok: InvokeRes = serde_json::from_str(r#"{"value":{"a":1},"error":""}"#).unwrap();
		assert!(ok.error.is_empty());
		assert_eq!(ok.value["a"], 1);

		let err: InvokeRes = serde_json::from_str(r#"{"value":null,"error":"boom"}"#).unwrap();
		assert_eq!(err.error, "boom");
		assert!(err.value.is_null());
	}
}
