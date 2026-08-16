//! The daemon-side KernRpc server: the request handler and the accept loop on
//! the per-cwd socket — how local clients (CLI, other daemons) talk to the
//! daemon that owns a store. The socket lives in the daemon's own data dir, so
//! filesystem ownership is the access model; there is no token handshake.

// The health envelope alone: `json!` recurses once per key, and the health
// surface outgrew the default 128 when `ingest_hygiene_rejected` landed.
#![recursion_limit = "256"]

use std::sync::Arc;

use serde_json::Value;
use transport::kern_rpc::{serve_kern_rpc, HealthRes, InvokeReq, InvokeRes, KernRpc, ShutdownRes};
use transport::typed::{Channel, JsonEnvelopeCodec, LocalListener};

use crate::server::Server;

#[derive(Clone)]
pub struct KernRpcHandler {
	pub kern: Arc<Server>,
	// Fires the daemon's graceful-exit path (save then exit) — the hub's unload.
	pub shutdown: Arc<tokio::sync::Notify>,
}

impl KernRpcHandler {
	pub fn new(kern: Arc<Server>, shutdown: Arc<tokio::sync::Notify>) -> Self {
		Self { kern, shutdown }
	}
}

impl KernRpc for KernRpcHandler {
	fn shutdown(&self) -> impl ::core::future::Future<Output = ShutdownRes> + Send {
		let notify = self.shutdown.clone();
		async move {
			notify.notify_one();
			ShutdownRes { ok: true }
		}
	}

	fn health(&self) -> impl ::core::future::Future<Output = HealthRes> + Send {
		let kern = self.kern.clone();
		async move {
			let payload = kern.health_stats();
			let kerns = payload.get("kerns").and_then(|v| v.as_u64()).unwrap_or(0);
			let entities = payload
				.get("entities")
				.and_then(|v| v.as_u64())
				.unwrap_or(0);
			let data_dir = payload
				.get("data_dir")
				.and_then(|v| v.as_str())
				.unwrap_or("")
				.to_string();
			let u64_at = |k: &str| payload.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
			let str_at = |k: &str| {
				payload
					.get(k)
					.and_then(|v| v.as_str())
					.unwrap_or("")
					.to_string()
			};
			HealthRes {
				ok: true,
				data_dir,
				kerns,
				entities,
				idle_ms: kern.idle_ms(),
				queue_depth: u64_at("queue_depth"),
				tasks_done: u64_at("tasks_done"),
				task_avg_ms: u64_at("task_avg_ms"),
				task_panics: u64_at("task_panics"),
				last_task_panic: str_at("last_task_panic"),
				task_failures: u64_at("task_failures"),
				last_task_failure: str_at("last_task_failure"),
				cold_evicted: u64_at("cold_evicted"),
				query_dim_rejected: u64_at("query_dim_rejected"),
				below_floor_deliveries: u64_at("below_floor_deliveries"),
				clock_skew_skips: u64_at("clock_skew_skips"),
				ingest_dropped_chunks: u64_at("ingest_dropped_chunks"),
				unspilled_drops: u64_at("unspilled_drops"),
				ingest_queue_refused: u64_at("ingest_queue_refused"),
				ingest_queue_depth: u64_at("ingest_queue_depth"),
				gini_access: payload
					.get("gini_access")
					.and_then(|v| v.as_f64())
					.unwrap_or(0.0),
				max_kerns: u64_at("max_kerns"),
				gnn_train_refused: u64_at("gnn_train_refused"),
				supersede_chain_depth_exceeded: u64_at("supersede_chain_depth_exceeded"),
				largest_kern_entities: payload
					.get("largest_kern_entities")
					.and_then(|v| v.as_u64())
					.unwrap_or(0) as usize,
				gini_kern_sizes: payload
					.get("gini_kern_sizes")
					.and_then(|v| v.as_f64())
					.unwrap_or(0.0),
				heat_half_life_secs: u64_at("heat_half_life_secs"),
				qbst_recency_half_life_secs: u64_at("qbst_recency_half_life_secs"),
				retrieval: {
					let r = payload.get("retrieval");
					let mw = |key: &str| transport::kern_rpc::ModeWeightsHealth {
						content: r
							.and_then(|r| r.get(key))
							.and_then(|w| w.get("content"))
							.and_then(|v| v.as_f64())
							.unwrap_or(0.0),
						reason: r
							.and_then(|r| r.get(key))
							.and_then(|w| w.get("reason"))
							.and_then(|v| v.as_f64())
							.unwrap_or(0.0),
						edge: r
							.and_then(|r| r.get(key))
							.and_then(|w| w.get("edge"))
							.and_then(|v| v.as_f64())
							.unwrap_or(0.0),
					};
					transport::kern_rpc::RetrievalHealth {
						rrf_k: r
							.and_then(|r| r.get("rrf_k"))
							.and_then(|v| v.as_f64())
							.unwrap_or(0.0),
						rrf_global_weight: r
							.and_then(|r| r.get("rrf_global_weight"))
							.and_then(|v| v.as_f64())
							.unwrap_or(0.0),
						weights_content: mw("weights_content"),
						weights_reason: mw("weights_reason"),
						weights_hybrid: mw("weights_hybrid"),
						seed_k: r
							.and_then(|r| r.get("seed_k"))
							.and_then(|v| v.as_u64())
							.unwrap_or(0) as usize,
						mmr_enabled: r
							.and_then(|r| r.get("mmr_enabled"))
							.and_then(|v| v.as_bool())
							.unwrap_or(false),
						lexical_enabled: r
							.and_then(|r| r.get("lexical_enabled"))
							.and_then(|v| v.as_bool())
							.unwrap_or(false),
						pagerank_enabled: r
							.and_then(|r| r.get("pagerank_enabled"))
							.and_then(|v| v.as_bool())
							.unwrap_or(false),
					}
				},
				preset: str_at("preset").to_string(),
				source_trust: payload
					.get("source_trust")
					.and_then(|v| v.as_object())
					.map(|obj| {
						obj
							.iter()
							.filter_map(|(k, v)| v.as_f64().map(|w| (k.clone(), w)))
							.collect()
					})
					.unwrap_or_default(),
				ingest_dedup_threshold: payload
					.get("ingest_dedup_threshold")
					.and_then(|v| v.as_f64())
					.unwrap_or(0.0),
				ingest_dedup_threshold_by_kind: payload
					.get("ingest_dedup_threshold_by_kind")
					.and_then(|v| v.as_array())
					.map(|arr| {
						let mut out = [None; 5];
						for (i, v) in arr.iter().take(5).enumerate() {
							out[i] = v.as_f64();
						}
						out
					})
					.unwrap_or([None; 5]),
				llm_complete_failed: u64_at("llm_complete_failed"),
				last_llm_complete_failure: str_at("last_llm_complete_failure"),
				embed_model: str_at("embed_model"),
				embed_dim: u64_at("embed_dim"),
				embed_mismatch: payload
					.get("embed_mismatch")
					.and_then(|v| v.as_bool())
					.unwrap_or(false),
				build_id: identity::build_id(),
				config_id: identity::config_id(&kern.cfg),
				uptime_ms: identity::uptime_ms(),
			}
		}
	}

	fn invoke(&self, req: InvokeReq) -> impl ::core::future::Future<Output = InvokeRes> + Send {
		let kern = self.kern.clone();
		async move {
			match kern.invoke(&req.name, &req.args) {
				Ok(value) => InvokeRes {
					value,
					error: String::new(),
				},
				Err(error) => InvokeRes {
					value: Value::Null,
					error,
				},
			}
		}
	}
}

/// Serve one connection: dispatch `KernRpc` methods until the peer goes away.
pub async fn serve_kern_rpc_loop(mut listener: LocalListener, handler: KernRpcHandler) {
	loop {
		let adapter = match listener.accept().await {
			Ok(a) => a,
			Err(e) => {
				tracing::warn!(target: "kern.kern_rpc", error = %e, "accept");
				continue;
			}
		};
		let handler = handler.clone();
		tokio::spawn(async move {
			let channel = Channel::new(adapter, JsonEnvelopeCodec::new());
			let served = serve_kern_rpc(channel, handler).await;
			if let Err(e) = served {
				tracing::warn!(target: "kern.kern_rpc", error = %e, "serve loop");
			}
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tick::tick_queue::{task, Queue, TaskKind};

	#[tokio::test]
	async fn health_carries_every_degradation_signal_to_the_rpc_surface() {
		let mut srv = crate::test_helpers::server();
		let q = Arc::new(Queue::new(8));
		q.record_task_panic(&task(TaskKind::Cluster, "k1"), "boom");
		q.record_task_failure(&task(TaskKind::GnnPropagate, "k2"), "train epoch 0 forward");
		srv.task_q = Some(q);

		let handler = KernRpcHandler::new(Arc::new(srv), Arc::new(tokio::sync::Notify::new()));
		let res = handler.health().await;

		assert!(res.ok);
		assert_eq!(res.task_panics, 1);
		assert_eq!(res.last_task_panic, "Cluster[k1]: boom");
		assert_eq!(res.task_failures, 1);
		assert_eq!(
			res.last_task_failure,
			"GnnPropagate[k2]: train epoch 0 forward"
		);
		assert_eq!(res.cold_evicted, 0);
		assert!(!res.embed_mismatch);
	}

	// Not "the key is present" — a real refusal, walked from the worker's counter
	// through the health stats to the RPC DTO an operator polls.
	#[tokio::test]
	async fn a_refused_ingest_reaches_the_rpc_health_surface() {
		let _serial = ingest::worker::queue_refused_test_lock().lock().await;
		let (url, _server) = test_support::spawn_http(test_support::hanging_embed_app()).await;
		let srv = crate::test_helpers::server_with_embed_url(&url);

		let mut offered = 0;
		while srv
			.worker
			.enqueue(
				format!("filler {offered}"),
				base::base_types::Source::Inline {
					hash: String::new(),
					section: String::new(),
				},
				base::base_types::EntityKind::Claim,
				String::new(),
				1.0,
				"inline",
				ingest::Config::default(),
				base::base_types::Scoping::default(),
			)
			.is_some()
		{
			offered += 1;
			tokio::task::yield_now().await;
			assert!(offered < 10_000, "the queue never filled");
		}

		let handler = KernRpcHandler::new(Arc::new(srv), Arc::new(tokio::sync::Notify::new()));
		let h = handler.health().await;
		assert!(
			h.ingest_queue_refused >= 1,
			"a refused ingest that no health surface reports is a lost write nobody can see"
		);
		// The queue that just refused is full, and the gauge is what says so: a
		// handler hardcoding the field to 0 still compiles and still reports healthy.
		assert!(
			h.ingest_queue_depth >= 1,
			"a full queue that reports depth 0 hides the backlog behind the refusals"
		);
	}

	// Same shape, for the trainer's cap: a real refusal walked from the trainer's
	// counter through the health stats to the RPC DTO. Nonzero on purpose — a
	// handler that hardcodes the field to `0` still names it, still compiles, and
	// still reports a healthy daemon.
	#[tokio::test]
	async fn a_refused_propagation_reaches_the_rpc_health_surface() {
		use tick_loop::tick_trainer::{gnn_train_refused, Submit, Trainer, REFUSAL_COUNTER};

		// Held first, so it outlives the trainer: this test fills a queue and so
		// refuses a whole cap's worth, and `TRAIN_REFUSED` is one global for the
		// process `cargo test` runs the suite in. Without this the trainer's own
		// cap test — which asserts its delta is exactly 1 — reds on 1 run in 6.
		let _serial = REFUSAL_COUNTER.lock().await;

		// A runner that blocks until its sender drops, so the queue fills and stays
		// full instead of draining out from under the test.
		let (release, gate) = std::sync::mpsc::sync_channel::<()>(0);
		let trainer = Trainer::spawn(Arc::new(Queue::new(8)), move |_| {
			let _ = gate.recv();
		});
		let mut offered = 0;
		while trainer.submit(&format!("k{offered}")) != Submit::Refused {
			offered += 1;
			assert!(offered < 10_000, "the trainer queue never filled");
		}

		let handler = KernRpcHandler::new(
			Arc::new(crate::test_helpers::server()),
			Arc::new(tokio::sync::Notify::new()),
		);
		let refused = gnn_train_refused();
		assert!(refused >= 1, "the refusal itself was never counted");
		assert_eq!(
			handler.health().await.gnn_train_refused,
			refused,
			"a refused propagation no health surface reports is a kern left on stale embeddings nobody can see"
		);
		drop(release);
	}
}

pub mod server;
pub mod test_helpers;
