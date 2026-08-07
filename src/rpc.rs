//! The daemon-side KernRpc server: auth requirements derived from config, the
//! request handler, and the accept loop on the per-cwd socket — how local
//! clients (CLI, other daemons) talk to the daemon that owns a store.

use std::path::{Path, PathBuf};

use crate::transport::kern_rpc::AuthReq;

/// The secret a client presents to the daemon rooted at `root`.
///
/// Two lookups, and the second one is the whole point: the *socket* is keyed by
/// the root (`Endpoint::kern_for` hashes the path) while the *token* is keyed by
/// the data_dir. Those diverge the moment kern.toml is repointed under a live
/// daemon — the daemon keeps serving out of the store it opened at boot, and a
/// later CLI, reading the new config, would go looking for the secret in a
/// directory that daemon never wrote to. So: the configured store first, the
/// root's conventional `.kern/data` second.
///
/// Searching costs nothing to be wrong about. The daemon compares against the
/// secret *it* resolved, so the only outcome of guessing badly is a refusal —
/// this is a client hunting for a key, never a server deciding to accept one.
fn token_for(root: &Path, cfg: &config::Config) -> Option<String> {
	cfg
		.serve
		.read_mcp_token(Path::new(&cfg.data_dir))
		.or_else(|| {
			let conventional = config::Config::default_in(root).data_dir;
			(conventional != cfg.data_dir)
				.then(|| cfg.serve.read_mcp_token(Path::new(&conventional)))
				.flatten()
		})
}

fn cwd() -> PathBuf {
	std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// What a client presents on connect: the graph's `mcp-token`. For the commands
/// that run pinned to the project root, which is all of them — `main.rs` re-pins
/// cwd before dispatch.
///
/// No token found leaves it empty, and the daemon refuses an empty token like
/// any other wrong one: a caller that cannot prove itself is turned away, not
/// waved through.
pub fn caller_of(cfg: &config::Config) -> AuthReq {
	AuthReq::new(token_for(&cwd(), cfg).unwrap_or_default())
}

/// The same, for a caller that knows only which directory the graph lives in —
/// the hub, whose nodes are addressed by a root it never stands in.
pub fn caller_at(root: &Path) -> AuthReq {
	let cfg = config::Config::load(root).unwrap_or_else(|_| config::Config::default_in(root));
	AuthReq::new(token_for(root, &cfg).unwrap_or_default())
}

/// The caller identity for the graph rooted at this process's cwd — the root
/// `Endpoint::kern()` resolves against.
pub fn caller() -> AuthReq {
	caller_at(&cwd())
}

#[cfg(test)]
mod caller_tests {
	use super::*;
	use config::mcp_token_path;

	fn write_token(data_dir: &Path, token: &str) {
		std::fs::create_dir_all(data_dir).unwrap();
		std::fs::write(mcp_token_path(data_dir), token).unwrap();
	}

	#[test]
	fn the_configured_store_is_where_the_token_is_read_from() {
		let root = tempfile::tempdir().unwrap();
		let mut cfg = config::Config::default_in(root.path());
		cfg.data_dir = root.path().join("store").to_string_lossy().into_owned();
		write_token(Path::new(&cfg.data_dir), "configured-secret");
		assert_eq!(
			token_for(root.path(), &cfg).as_deref(),
			Some("configured-secret")
		);
	}

	// The e2e blinding shape, and the reason the fallback exists: the daemon
	// booted on the conventional store, then kern.toml was repointed. The socket
	// is still the root's, so the caller must still find the root's secret.
	#[test]
	fn a_repointed_data_dir_still_finds_the_secret_the_root_daemon_minted() {
		let root = tempfile::tempdir().unwrap();
		let conventional = config::Config::default_in(root.path()).data_dir;
		write_token(Path::new(&conventional), "the-daemons-secret");

		let mut cfg = config::Config::default_in(root.path());
		cfg.data_dir = root.path().join("blind").to_string_lossy().into_owned();
		assert_eq!(
			token_for(root.path(), &cfg).as_deref(),
			Some("the-daemons-secret"),
			"a blinded CLI must still reach the daemon it is supposed to route to"
		);
	}

	#[test]
	fn no_token_anywhere_yields_none_rather_than_something_forgiving() {
		let root = tempfile::tempdir().unwrap();
		let cfg = config::Config::default_in(root.path());
		assert!(token_for(root.path(), &cfg).is_none());
		assert!(
			caller_at(root.path()).token.is_empty(),
			"an absent secret presents as empty, which verify_auth refuses"
		);
	}

	// A file that exists but says nothing is the same as no file. Neither a
	// zero-length token nor one that is only whitespace may become the empty
	// string a caller then presents — `verify_auth` refuses an empty `expected`,
	// but an empty *offered* token against a real secret must be refused by the
	// compare, and the cheapest way to be sure is never to mint one.
	#[test]
	fn a_blank_token_file_reads_as_no_token_at_all() {
		for body in ["", "   ", "\n", " \t\r\n "] {
			let root = tempfile::tempdir().unwrap();
			let cfg = config::Config::default_in(root.path());
			write_token(Path::new(&cfg.data_dir), body);
			assert!(
				token_for(root.path(), &cfg).is_none(),
				"a blank mcp-token must not present as a token ({body:?})"
			);
			assert!(caller_at(root.path()).token.is_empty());
		}
	}

	#[test]
	fn an_explicit_configured_token_wins_and_needs_no_file() {
		let root = tempfile::tempdir().unwrap();
		let mut cfg = config::Config::default_in(root.path());
		cfg.serve.mcp_token = "explicit".into();
		assert_eq!(token_for(root.path(), &cfg).as_deref(), Some("explicit"));
	}
}

use std::sync::Arc;

use crate::transport::kern_rpc::{
	serve_kern_rpc, verify_auth, CallToolReq, CallToolRes, HealthRes, KernRpc, ListToolsReq,
	ListToolsRes, ShutdownRes,
};
use crate::transport::typed::{AdapterError, Channel, JsonEnvelopeCodec, LocalListener};
use crate::transport::McpServer;
use serde_json::Value;

#[derive(Clone)]
pub struct KernRpcHandler {
	pub kern: Arc<crate::mcp::Server>,
	// Fires the daemon's graceful-exit path (save then exit) — the hub's unload.
	pub shutdown: Arc<tokio::sync::Notify>,
}

impl KernRpcHandler {
	pub fn new(kern: Arc<crate::mcp::Server>, shutdown: Arc<tokio::sync::Notify>) -> Self {
		Self { kern, shutdown }
	}
}

fn unwrap_tool_json(envelope: &Value) -> Result<Value, String> {
	if envelope.get("isError").and_then(|v| v.as_bool()) == Some(true) {
		let msg = envelope
			.pointer("/content/0/text")
			.and_then(|v| v.as_str())
			.unwrap_or("kern tool error")
			.to_string();
		return Err(msg);
	}
	let text = envelope
		.pointer("/content/0/text")
		.and_then(|v| v.as_str())
		.unwrap_or("");
	if text.is_empty() {
		return Ok(Value::Null);
	}
	serde_json::from_str(text).map_err(|e| format!("decode tool result: {e}"))
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
			let env = kern.tool_health();
			let payload = match unwrap_tool_json(&env) {
				Ok(v) => v,
				Err(_) => return HealthRes::default(),
			};
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
				remote_cap_dropped: u64_at("remote_cap_dropped"),
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
					let mw = |key: &str| crate::transport::kern_rpc::ModeWeightsHealth {
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
					crate::transport::kern_rpc::RetrievalHealth {
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
				build_id: gossip::identity::build_id(),
				config_id: gossip::identity::config_id(&kern.cfg),
				uptime_ms: gossip::identity::uptime_ms(),
			}
		}
	}

	fn call_tool(
		&self,
		req: CallToolReq,
	) -> impl ::core::future::Future<Output = CallToolRes> + Send {
		let kern = self.kern.clone();
		async move {
			// Forward to the single MCP `call_tool` dispatcher so the proxy relays any
			// `tools/call` over kern.sock without enumerating every tool.
			let envelope = match McpServer::call_tool(&*kern, &req.name, &req.args) {
				Ok(tr) => serde_json::json!({
						"content": tr.content,
						"isError": tr.is_error,
				}),
				Err(e) => serde_json::json!({
						"content": [{
								"type": "text",
								"text": format!("kern_rpc::call_tool: {e}"),
						}],
						"isError": true,
				}),
			};
			CallToolRes { envelope }
		}
	}

	fn list_tools(
		&self,
		_req: ListToolsReq,
	) -> impl ::core::future::Future<Output = ListToolsRes> + Send {
		let kern = self.kern.clone();
		async move {
			// Serialise the live `tools/list` to raw schemas so the proxy advertises
			// exactly what we expose.
			let tools = McpServer::tools_list(&*kern)
				.iter()
				.map(|s| s.to_value())
				.collect();
			ListToolsRes { tools }
		}
	}
}

/// Gate first, dispatch second.
///
/// `make_handler` runs only after the token verifies, so the ordering is
/// structural rather than a convention someone has to remember: on an
/// unauthenticated connection there is no handler in existence for a `KernRpc`
/// method to be dispatched to.
pub(crate) async fn serve_authenticated<H, F>(
	mut channel: Channel<JsonEnvelopeCodec>,
	token: &str,
	make_handler: F,
) -> Result<(), AdapterError>
where
	H: KernRpc,
	F: FnOnce() -> H,
{
	verify_auth(&mut channel, token).await?;
	serve_kern_rpc(channel, make_handler()).await
}

/// `token` is the graph's `mcp-token` (`ServeConfig::resolve_mcp_token`). It is
/// taken by value because every connection needs it for the life of the loop —
/// and if it ever arrives empty, `verify_auth` serves nobody rather than
/// everybody.
pub async fn serve_kern_rpc_loop(
	mut listener: LocalListener,
	handler: KernRpcHandler,
	token: String,
) {
	let token = Arc::new(token);
	loop {
		let adapter = match listener.accept().await {
			Ok(a) => a,
			Err(e) => {
				tracing::warn!(target: "kern.kern_rpc", error = %e, "accept");
				continue;
			}
		};
		let handler = handler.clone();
		let token = token.clone();
		tokio::spawn(async move {
			let channel = Channel::new(adapter, JsonEnvelopeCodec::new());
			let served = serve_authenticated(channel, &token, || {
				tracing::debug!(target: "kern.kern_rpc", "authenticated");
				handler
			})
			.await;
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
		let mut srv = crate::test_support::mcp_server();
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
	// through the health stats and the MCP payload to the RPC DTO an operator polls.
	#[tokio::test]
	async fn a_refused_ingest_reaches_the_rpc_health_surface() {
		let _serial = ingest::worker::queue_refused_test_lock()
			.lock()
			.await;
		let (url, _server) =
			crate::test_support::spawn_http(crate::test_support::hanging_embed_app()).await;
		let srv = crate::test_support::mcp_server_with_embed_url(&url);

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
	// counter through the MCP payload to the RPC DTO. Nonzero on purpose — a
	// handler that hardcodes the field to `0` still names it, still compiles, and
	// still reports a healthy daemon.
	#[tokio::test]
	async fn a_refused_gnn_training_reaches_the_rpc_health_surface() {
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
			Arc::new(crate::test_support::mcp_server()),
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

// The gate itself. Not "the handshake round-trips" — that lives in transport —
// but the thing an unauthenticated caller is trying to reach: `call_tool`.
#[cfg(test)]
mod auth_gate_tests {
	use super::*;
	use crate::transport::kern_rpc::AuthReq;
	use crate::transport::typed::InprocAdapter;
	use std::sync::atomic::{AtomicUsize, Ordering};

	const TOKEN: &str = "the-real-token";

	// Counts what got through. `call_tool` incrementing at all on a refused
	// connection is the failure — a gate that runs the tool and then complains
	// has already done the damage.
	#[derive(Clone, Default)]
	struct Spy {
		calls: Arc<AtomicUsize>,
	}

	impl KernRpc for Spy {
		async fn shutdown(&self) -> ShutdownRes {
			ShutdownRes { ok: true }
		}
		async fn health(&self) -> HealthRes {
			HealthRes {
				ok: true,
				..Default::default()
			}
		}
		fn call_tool(
			&self,
			_req: CallToolReq,
		) -> impl ::core::future::Future<Output = CallToolRes> + Send {
			let calls = self.calls.clone();
			async move {
				calls.fetch_add(1, Ordering::SeqCst);
				CallToolRes {
					envelope: serde_json::json!({ "ok": true }),
				}
			}
		}
		async fn list_tools(&self, _req: ListToolsReq) -> ListToolsRes {
			ListToolsRes { tools: vec![] }
		}
	}

	fn call_tool_frame() -> Value {
		serde_json::json!({
			"id": 1,
			"method": "call_tool",
			"params": { "req": { "name": "health", "args": {} } },
		})
	}

	/// Drive one connection through the real gate. `auth` is what the client
	/// sends first — `None` means it sends nothing and goes straight for the
	/// tool, which is exactly what an unauthenticated caller would do.
	///
	/// The tool call is offered *twice*, and that is deliberate. A gate that
	/// consumes the first frame as a handshake would swallow a single attempt
	/// whether it verified anything or not, so one attempt cannot tell an open
	/// gate from a closed one. Two can: past an open gate the second lands.
	async fn attempt(auth: Option<AuthReq>) -> (usize, Option<Value>) {
		let (server_side, client_side) = InprocAdapter::pair();
		let calls = Arc::new(AtomicUsize::new(0));
		let spy_calls = calls.clone();
		let server = tokio::spawn(async move {
			let channel = Channel::new(server_side, JsonEnvelopeCodec::new());
			let _ = serve_authenticated(channel, TOKEN, move || Spy { calls: spy_calls }).await;
		});

		let mut client = Channel::new(client_side, JsonEnvelopeCodec::new());
		if let Some(auth) = auth {
			let _ = client.send(serde_json::json!({ "auth": auth })).await;
			let _ = client.recv().await; // the verdict frame
		}
		let mut result = None;
		for _ in 0..2 {
			if client.send(call_tool_frame()).await.is_err() {
				break;
			}
			match client.recv().await {
				Ok(Some(frame)) => {
					if let Some(r) = frame.get("result") {
						result = Some(r.clone());
						break;
					}
				}
				_ => break,
			}
		}
		drop(client);
		let _ = server.await;
		(calls.load(Ordering::SeqCst), result)
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn a_connection_with_no_token_never_reaches_call_tool() {
		let (calls, result) = attempt(None).await;
		assert_eq!(
			calls, 0,
			"an unauthenticated caller ran a tool — the gate is decoration"
		);
		assert!(result.is_none(), "and it must get no result back either");
	}

	// Two wrong tokens, and the second is the one with teeth. `not-the-token` is
	// a byte shorter than `TOKEN`, so the length check inside `ct_eq` refuses it
	// without ever comparing a byte — on its own it leaves the compare untested,
	// and gutting the compare's body kills nothing here. `the-real-tokex` is the
	// same length and differs only in the final byte: only a compare that runs
	// to the end refuses it.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_connection_with_the_wrong_token_never_reaches_call_tool() {
		for offered in ["not-the-token", "the-real-tokex"] {
			let (calls, result) = attempt(Some(AuthReq::new(offered))).await;
			assert_eq!(
				calls, 0,
				"a wrong token got a tool call through: {offered:?}"
			);
			assert!(result.is_none(), "offered {offered:?}");
		}
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn the_right_token_gets_through() {
		let (calls, result) = attempt(Some(AuthReq::new(TOKEN))).await;
		assert_eq!(calls, 1, "the daemon's own clients must still be served");
		let res = result.expect("an authenticated call_tool answers");
		assert_eq!(
			res.pointer("/envelope/ok").and_then(Value::as_bool),
			Some(true),
			"the handler's answer travels back whole"
		);
	}

	// The gate holds resources, not just decisions: item 24 puts `verify_auth`
	// ahead of `make_handler`, so a connection parked in the handshake is a
	// spawned task and an fd held for a session that will never be authorised.
	// The assertion is that the serve future *finishes* — a deadline that
	// refuses but leaves the task alive has reclaimed nothing.
	#[tokio::test(flavor = "current_thread", start_paused = true)]
	async fn a_connection_that_opens_and_says_nothing_releases_its_slot() {
		let (server_side, client_side) = InprocAdapter::pair();
		let calls = Arc::new(AtomicUsize::new(0));
		let spy_calls = calls.clone();
		let mut client = Channel::new(client_side, JsonEnvelopeCodec::new());

		let served = tokio::time::timeout(std::time::Duration::from_secs(60), async move {
			let channel = Channel::new(server_side, JsonEnvelopeCodec::new());
			serve_authenticated(channel, TOKEN, move || Spy { calls: spy_calls }).await
		})
		.await
		.expect("a silent connection kept the serve loop alive with no deadline to end it");

		assert!(served.is_err(), "and it is refused, not served");
		assert_eq!(calls.load(Ordering::SeqCst), 0);
		// The far end is gone: the handshake dropped the channel rather than
		// parking on it, so the client's next frame has nowhere to land.
		let _ = client.send(call_tool_frame()).await;
		assert!(
			client.recv().await.unwrap_or(None).is_none(),
			"the daemon side must be closed, not idling"
		);
	}

	// The compatibility half: past the gate, the wire is byte-for-byte what it
	// was. Compared against the same handler called in-process, so a drift in
	// the envelope shows up here rather than in an agent's tool output.
	#[tokio::test(flavor = "multi_thread")]
	async fn an_authenticated_call_answers_exactly_what_the_handler_answers_directly() {
		let handler = KernRpcHandler::new(
			Arc::new(crate::test_support::mcp_server()),
			Arc::new(tokio::sync::Notify::new()),
		);
		let direct = handler
			.call_tool(CallToolReq {
				name: "health".into(),
				args: serde_json::json!({}),
			})
			.await
			.envelope;

		let (server_side, client_side) = InprocAdapter::pair();
		let gated = handler.clone();
		let server = tokio::spawn(async move {
			let channel = Channel::new(server_side, JsonEnvelopeCodec::new());
			let _ = serve_authenticated(channel, TOKEN, || gated).await;
		});

		let mut client = Channel::new(client_side, JsonEnvelopeCodec::new());
		client
			.send(serde_json::json!({ "auth": AuthReq::new(TOKEN) }))
			.await
			.unwrap();
		client.recv().await.unwrap().expect("a verdict frame");
		client.send(call_tool_frame()).await.unwrap();
		let frame = client.recv().await.unwrap().expect("a reply frame");
		drop(client);
		let _ = server.await;

		let over_the_wire = frame
			.pointer("/result/envelope")
			.cloned()
			.expect("call_tool result");
		assert_eq!(
			serde_json::to_string(&over_the_wire).unwrap(),
			serde_json::to_string(&direct).unwrap(),
			"the gate must add nothing to and take nothing from the answer"
		);
	}
}
