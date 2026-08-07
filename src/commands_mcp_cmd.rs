//! The `mcp` subcommand: run the MCP server over stdio against a store, or
//! proxy to the daemon that owns it.

use parking_lot::RwLock as StdRwLock;
use std::sync::Arc;

use crate::transport::kern_rpc::{AuthReq, CallToolReq, KernRpcClient};
use crate::transport::typed::{AdapterError, Endpoint, JsonEnvelopeCodec};
use crate::transport::{McpError, McpServer, ToolResult, ToolSchema};
use tokio::sync::Mutex as TokioMutex;

use crate::commands::load_graph;

pub(crate) async fn cmd_mcp(cfg: &crate::config::Config) {
	// Hub-first: a running hub owns node lifecycle (spawn, adopt, unload) so the
	// proxy never self-spawns a daemon the hub can't see. No hub -> direct path.
	let log_dir = cfg.log_dir();
	let caller = crate::rpc::caller_of(cfg);
	if let Some(client) = attach_via_hub(cfg.hub.auto_start, &log_dir).await {
		let client = replace_if_stale(client, cfg, &log_dir, true).await;
		run_proxy(client, caller).await;
		return;
	}
	match attach_with_retry(&caller, 2, 150).await {
		Ok(client) => {
			let client = replace_if_stale(client, cfg, &log_dir, false).await;
			run_proxy(client, caller).await;
		}
		Err(e_first) => {
			tracing::info!(
				target: "kern.mcp",
				error = %e_first,
				"no daemon at kern.sock — auto-spawning detached daemon"
			);
			match spawn_daemon(&log_dir) {
				Ok(()) => match attach_with_retry(&caller, 6, 150).await {
					Ok(client) => {
						tracing::info!(
							target: "kern.mcp_proxy",
							"attached to auto-spawned daemon — proxy mode"
						);
						run_proxy(client, caller).await;
					}
					Err(e_retry) => {
						tracing::warn!(
							target: "kern.mcp",
							error = %e_retry,
							"auto-spawn failed, falling back to standalone"
						);
						run_standalone(cfg).await;
					}
				},
				Err(e_spawn) => {
					tracing::warn!(
						target: "kern.mcp",
						error = %e_spawn,
						"auto-spawn failed, falling back to standalone"
					);
					run_standalone(cfg).await;
				}
			}
		}
	}
}

async fn run_proxy(client: KernRpcClient<JsonEnvelopeCodec>, auth: AuthReq) {
	tracing::info!(
		target: "kern.mcp_proxy",
		"attached to running daemon — proxy mode"
	);
	let proxy = ProxyServer {
		client: Arc::new(TokioMutex::new(client)),
		auth,
	};
	// serve_stdio is sync — on a blocking thread so it doesn't park a worker;
	// call_tool crosses back via block_in_place (multi-thread rt only).
	if let Err(e) = tokio::task::spawn_blocking(move || crate::transport::serve_stdio(&proxy)).await {
		tracing::warn!(target: "kern.mcp_proxy", error = %e, "stdio loop");
	}
}

// One attempt per invocation, by construction: this runs once on the way into
// the proxy and never loops. A client whose replacement is itself stale
// proxies anyway rather than restarting again.
async fn replace_if_stale(
	client: KernRpcClient<JsonEnvelopeCodec>,
	cfg: &crate::config::Config,
	log_dir: &std::path::Path,
	via_hub: bool,
) -> KernRpcClient<JsonEnvelopeCodec> {
	use crate::commands_mcp_restart::{verdict, Verdict};
	use crate::identity;

	let caller = crate::rpc::caller_of(cfg);

	let health = match client.health().await {
		Ok(h) => h,
		// A daemon that will not answer health is not one to judge stale.
		Err(_) => return client,
	};
	let (my_build, my_config) = (identity::build_id(), identity::config_id(cfg));
	let reason = match verdict(&health, &my_build, &my_config) {
		Verdict::Fresh => return client,
		Verdict::Hold(why) => {
			if health.build_id != my_build || health.config_id != my_config {
				tracing::warn!(
					target: "kern.mcp",
					reason = why,
					daemon_build = %health.build_id,
					client_build = %my_build,
					"attached to a daemon that does not match this client — not restarting"
				);
			}
			return client;
		}
		Verdict::Stale(why) => why,
	};

	if !cfg.hub.auto_restart {
		tracing::warn!(
			target: "kern.mcp",
			reason,
			"stale daemon — set [hub] auto_restart = true to replace it automatically"
		);
		return client;
	}

	tracing::info!(target: "kern.mcp", reason, "restarting stale daemon");
	// shutdown() fires the daemon's graceful path: drain, guarded flush, exit.
	// A refused or dropped call means it is already going down.
	let _ = client.shutdown().await;
	drop(client);

	// Wait for the socket to be released before anything re-binds it, or the
	// respawn loses the race to the corpse and we reattach to what we just killed.
	for _ in 0..40 {
		tokio::time::sleep(std::time::Duration::from_millis(150)).await;
		if attach_with_retry(&caller, 1, 0).await.is_err() {
			break;
		}
	}

	let fresh = if via_hub {
		attach_via_hub(cfg.hub.auto_start, log_dir).await
	} else {
		match spawn_daemon(log_dir) {
			Ok(()) => attach_with_retry(&caller, 40, 250).await.ok(),
			Err(e) => {
				tracing::warn!(target: "kern.mcp", error = %e, "respawn after restart failed");
				None
			}
		}
	};
	match fresh {
		Some(c) => {
			tracing::info!(target: "kern.mcp", "reattached to restarted daemon");
			c
		}
		// Fail open: no memory is recoverable, a dead proxy is not.
		None => {
			tracing::warn!(
				target: "kern.mcp",
				"could not reattach after restart — falling back to a fresh attach"
			);
			match attach_with_retry(&caller, 40, 250).await {
				Ok(c) => c,
				Err(e) => {
					tracing::error!(target: "kern.mcp", error = %e, "no daemon after restart");
					std::process::exit(1);
				}
			}
		}
	}
}

async fn attach_via_hub(
	auto_start: bool,
	log_dir: &std::path::Path,
) -> Option<KernRpcClient<JsonEnvelopeCodec>> {
	use crate::transport::hub_rpc::{HubRpcClient, ResolveReq};
	let hub = match HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
		Ok(h) => h,
		Err(_) if auto_start => {
			// Same detach pattern as spawn_daemon; a lost race lands on
			// AlreadyRunning in the second hub and the retry below still connects.
			if let Err(e) = spawn_hub(log_dir) {
				tracing::warn!(target: "kern.mcp", error = %e, "hub auto-start failed — direct path");
				return None;
			}
			let mut connected = None;
			for _ in 0..6 {
				tokio::time::sleep(std::time::Duration::from_millis(150)).await;
				if let Ok(h) = HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
					connected = Some(h);
					break;
				}
			}
			match connected {
				Some(h) => {
					tracing::info!(target: "kern.mcp", "auto-started machine hub");
					h
				}
				None => {
					tracing::warn!(target: "kern.mcp", "auto-started hub never answered — direct path");
					return None;
				}
			}
		}
		Err(_) => return None,
	};
	// main.rs re-pinned cwd to the project root before dispatch.
	let root = std::env::current_dir().ok()?;
	let res = hub
		.resolve(ResolveReq {
			root: root.display().to_string(),
		})
		.await
		.ok()?;
	if !res.ok {
		tracing::warn!(target: "kern.mcp", error = %res.err, "hub resolve failed — direct path");
		return None;
	}
	let endpoint = crate::transport::typed::Endpoint::parse(&res.endpoint);
	tracing::info!(
		target: "kern.mcp",
		endpoint = %res.endpoint,
		spawned = res.spawned,
		"attached via hub"
	);
	KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint(&endpoint, &crate::rpc::caller_at(&root))
		.await
		.ok()
}

async fn attach_with_retry(
	auth: &AuthReq,
	retries: u32,
	delay_ms: u64,
) -> Result<KernRpcClient<JsonEnvelopeCodec>, AdapterError> {
	let mut last_err: Option<AdapterError> = None;
	for i in 0..retries {
		match KernRpcClient::<JsonEnvelopeCodec>::connect_local(auth).await {
			Ok(c) => return Ok(c),
			Err(e) => {
				last_err = Some(e);
				if i + 1 < retries {
					tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
				}
			}
		}
	}
	Err(last_err.unwrap_or_else(|| AdapterError::Other("no attempts".into())))
}

fn spawn_hub(log_dir: &std::path::Path) -> std::io::Result<()> {
	spawn_detached("hub", log_dir)
}

fn spawn_daemon(log_dir: &std::path::Path) -> std::io::Result<()> {
	spawn_detached("--daemon", log_dir)
}

// Drop the child handle — detach flags + redirected stdio keep it alive past our exit.
fn spawn_detached(arg: &str, log_dir: &std::path::Path) -> std::io::Result<()> {
	use std::process::{Command, Stdio};
	let exe = std::env::current_exe()?;
	let (out, err) = crate::config_detached_log::stdio(log_dir, arg);
	let mut cmd = Command::new(exe);
	cmd.arg(arg).stdin(Stdio::null()).stdout(out).stderr(err);
	#[cfg(windows)]
	{
		use std::os::windows::process::CommandExt;
		const DETACHED_PROCESS: u32 = 0x0000_0008;
		const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
		cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
	}
	#[cfg(unix)]
	{
		use std::os::unix::process::CommandExt;
		cmd.process_group(0);
	}
	let _child = cmd.spawn()?;
	Ok(())
}

struct ProxyServer {
	client: Arc<TokioMutex<KernRpcClient<JsonEnvelopeCodec>>>,
	// Re-presented on the reconnect below: a successor daemon has never seen
	// this connection, so the proxy has to introduce itself again.
	auth: AuthReq,
}

impl McpServer for ProxyServer {
	fn server_name(&self) -> &str {
		"kern"
	}
	fn server_version(&self) -> &str {
		env!("CARGO_PKG_VERSION")
	}

	fn tools_list(&self) -> Vec<ToolSchema> {
		let client = self.client.clone();
		let res = crate::llm::block_on_in_place(async move {
			let c = client.lock().await;
			c.list_tools(crate::transport::kern_rpc::ListToolsReq {})
				.await
		});
		match res {
			Some(Ok(r)) => r
				.tools
				.into_iter()
				.filter_map(|v| crate::transport::ToolSchema::from_value(&v))
				.collect(),
			_ => crate::mcp::tools::typed_tool_schemas(),
		}
	}

	fn call_tool(&self, name: &str, args: &serde_json::Value) -> Result<ToolResult, McpError> {
		let client = self.client.clone();
		let auth = self.auth.clone();
		let req = CallToolReq {
			name: name.to_string(),
			args: args.clone(),
		};
		let res = crate::llm::block_on_in_place(async move {
			let first = {
				let c = client.lock().await;
				c.call_tool(req.clone()).await
			};
			match first {
				Ok(r) => Ok(r),
				// The daemon hot-reloads by handing its socket to a successor,
				// which severs established connections. Reconnect (riding out
				// the successor's graph load) and retry once. Safe to re-issue:
				// ingest is content-addressed and deduped, queries are reads.
				Err(_) => {
					let fresh = attach_with_retry(&auth, 40, 250).await?;
					let res = fresh.call_tool(req).await;
					*client.lock().await = fresh;
					res
				}
			}
		})
		.ok_or_else(|| McpError::Rpc {
			code: -32000,
			message: "kern_rpc call_tool: no tokio runtime".to_string(),
		})?
		.map_err(|e| McpError::Rpc {
			code: -32000,
			message: format!("kern_rpc call_tool: {e}"),
		})?;

		Ok(crate::mcp::value_to_tool_result(&res.envelope))
	}

	fn extra_capabilities(&self) -> serde_json::Value {
		// Must match the standalone server so a client probing capabilities
		// can't tell the two apart.
		serde_json::json!({"resources": {}, "prompts": {}})
	}

	// Answer everything `extra_capabilities` promises. Until 2026-07-22 this
	// override did not exist, so the trait default returned `None` and `dispatch`
	// turned every one of the five into `-32601` — on the path an agent actually
	// gets, since `cmd_mcp` reaches `run_proxy` whenever a daemon exists. Four of
	// the five answer from `handle_graphless_method`, the same function the
	// standalone dispatches through; `resources/read` needs the graph this
	// process does not hold and rides the `call_tool` passthrough.
	fn handle_method(
		&self,
		method: &str,
		params: serde_json::Value,
	) -> Option<Result<serde_json::Value, McpError>> {
		if let Some(r) = crate::mcp::handle_graphless_method(method, &params) {
			return Some(r);
		}
		match method {
			"resources/read" => Some(
				self
					.call_tool(crate::mcp::RESOURCE_READ_TOOL, &params)
					.and_then(|r| crate::mcp::decode_resource_read(&r)),
			),
			_ => None,
		}
	}
}

enum StandaloneEntry {
	Own(crate::lock::WriterLock),
	Attach(Box<KernRpcClient<JsonEnvelopeCodec>>),
	Refuse(String),
}

// The standalone fallback is the one long-lived writer with nothing watching it.
// It loads the graph once and flushes its own snapshot for hours, so a second
// one — or one beside a daemon that never answered the attach window — ends with
// the loser's whole graph landing last. It also has no socket of its own, so a
// sibling standalone is invisible to every probe; the writer lock is the only
// thing that can see it. Claim before the load, exactly as the lock requires.
async fn claim_standalone(
	data_dir: &str,
	endpoint: &Endpoint,
	auth: &AuthReq,
	retries: u32,
	delay: std::time::Duration,
) -> StandaloneEntry {
	let held = match crate::lock::acquire(data_dir, "mcp-standalone") {
		Ok(l) => return StandaloneEntry::Own(l),
		Err(e) => e,
	};
	// The likeliest holder is the daemon this process just spawned, up at last
	// but after the attach window closed. Proxying to it is strictly better than
	// dying, so spend one more window before refusing.
	match KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		endpoint, auth, retries, delay,
	)
	.await
	{
		Ok(c) => StandaloneEntry::Attach(Box::new(c)),
		Err(_) => StandaloneEntry::Refuse(held.to_string()),
	}
}

async fn run_standalone(cfg: &crate::config::Config) {
	let _writer_lock = match claim_standalone(
		&cfg.data_dir,
		&Endpoint::kern(),
		&crate::rpc::caller_of(cfg),
		40,
		std::time::Duration::from_millis(250),
	)
	.await
	{
		StandaloneEntry::Own(l) => l,
		StandaloneEntry::Attach(client) => return run_proxy(*client, crate::rpc::caller_of(cfg)).await,
		StandaloneEntry::Refuse(who) => {
			eprintln!("kern mcp: {who}");
			eprintln!(
				"  refusing to serve standalone — a second whole-graph writer overwrites the first"
			);
			std::process::exit(1);
		}
	};
	let g = Arc::new(StdRwLock::new(load_graph(cfg)));
	let llm_client = crate::commands::server_llm_client(cfg, cfg.reason_url(), &cfg.reason.model);
	// Long-lived writer: same stale-flush guard as the daemon — never overwrite
	// a graph another process grew on disk with a staler snapshot.
	let save_g = g.clone();
	let save_cfg = cfg.clone();
	let save_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
		crate::commands::save_graph_guarded(&save_g, &save_cfg);
	});
	let q = Arc::new(crate::tick_queue::Queue::new(512));
	let defer: crate::ingest::worker::DeferQuestionsFn = {
		let defer_q = q.clone();
		Arc::new(move |entity_id: &str| {
			let _ = defer_q.enqueue(crate::tick_queue::task_extra(
				crate::tick_queue::TaskKind::SeedQuestions,
				"",
				entity_id,
			));
		})
	};
	let defer_contradiction: crate::ingest::worker::DeferContradictionFn = {
		let contra_q = q.clone();
		Arc::new(move |kern_id: &str, reason_id: &str| {
			let _ = contra_q.enqueue(crate::tick_queue::task_extra(
				crate::tick_queue::TaskKind::ClassifyContradiction,
				kern_id,
				reason_id,
			));
		})
	};
	let worker = Arc::new(crate::ingest::Worker::new(
		g.clone(),
		llm_client.clone(),
		Some(defer),
		Some(defer_contradiction),
		Some(save_fn.clone()),
	));

	let tick_llm: crate::tick_tasks::LlmFunc = Arc::new(llm_client.complete_func());
	let tick_embed: crate::tick_tasks::EmbedFunc = {
		let c = llm_client.clone();
		Arc::new(move |text: &str| -> Result<Vec<f32>, String> {
			let c = c.clone();
			let text = text.to_string();
			match tokio::runtime::Handle::try_current() {
				Ok(h) => {
					let result = std::thread::scope(|_| h.block_on(c.embed(&text)));
					result.map_err(|e: crate::llm::LlmError| e.to_string())
				}
				Err(_) => Err("no runtime".to_string()),
			}
		})
	};
	crate::tick::start(
		q.clone(),
		g.clone(),
		crate::tick::TickContext {
			llm: Some(tick_llm),
			embed: Some(tick_embed),
			broadcast_q: None,
			gnn_cfg: cfg.gnn.into(),
			tick_cfg: cfg.tick,
			heat_cfg: cfg.heat,
		},
	);

	let server = crate::mcp::Server {
		graph: g,
		worker,
		llm: Some(llm_client),
		save_fn,
		task_q: Some(q),
		cfg: Arc::new(cfg.clone()),
		broadcast_pulse: None,
		last_activity: Arc::new(std::sync::atomic::AtomicU64::new(crate::util::now_ms())),
	};
	// KERN_TRANSPORT names one of the master transports (tcp:9401, ws:9402,
	// http:9403, sse:9404, udp:9405, unix:/path); absent, the MCP-child stdio
	// path is unchanged. serve_transport is blocking std threads, so it runs on
	// a blocking task rather than parking this worker.
	match transport_from_env() {
		Some(t) => {
			let server = Arc::new(server);
			if let Err(e) =
				tokio::task::spawn_blocking(move || crate::transport::serve_transport(t, server)).await
			{
				tracing::warn!(target: "kern.mcp", error = %e, "transport serve loop");
			}
		}
		None => server.run_stdio(),
	}
}

/// Parse `KERN_TRANSPORT` (`kind:arg`, e.g. `tcp:9401` or `unix:/tmp/k.sock`)
/// into a master transport, reusing `crate::transport::select`'s own flag grammar. An
/// unset or unparseable value falls through to stdio by returning `None`.
fn transport_from_env() -> Option<crate::transport::Transport> {
	let spec = std::env::var("KERN_TRANSPORT").ok()?;
	let (kind, arg) = spec.split_once(':')?;
	let args = [format!("--{kind}"), arg.to_string()];
	crate::transport::select(&args).ok()
}

// Idempotent: inserts only absent entries, never touches existing keys.
pub(crate) fn ensure_mcp_registered(cwd: &std::path::Path) {
	let mcp_path = cwd.join(".mcp.json");

	let raw = std::fs::read_to_string(&mcp_path).unwrap_or_else(|_| "{}".to_string());
	let mut root: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));

	let wanted: &[(&str, serde_json::Value)] = &[(
		"kern",
		serde_json::json!({"command": "kern", "args": ["mcp"]}),
	)];

	let servers = root.as_object_mut().map(|obj| {
		obj
			.entry("mcpServers")
			.or_insert_with(|| serde_json::json!({}))
	});

	let Some(servers) = servers.and_then(|s| s.as_object_mut()) else {
		tracing::warn!(target: "kern.mcp", "ensure_mcp_registered: mcpServers is not an object");
		return;
	};

	let mut changed = false;
	for (name, entry) in wanted {
		if !servers.contains_key(*name) {
			servers.insert(name.to_string(), entry.clone());
			changed = true;
		}
	}

	if !changed {
		return;
	}

	match serde_json::to_string_pretty(&root) {
		Ok(json) => {
			if let Err(e) = std::fs::write(&mcp_path, json) {
				tracing::warn!(target: "kern.mcp", error = %e, "ensure_mcp_registered: write failed");
			} else {
				tracing::info!(
						target: "kern.mcp",
						path = %mcp_path.display(),
						"registered kern MCP server in .mcp.json"
				);
			}
		}
		Err(e) => {
			tracing::warn!(target: "kern.mcp", error = %e, "ensure_mcp_registered: serialize failed")
		}
	}
}

// Item 9's serving half for the one process the RPC route cannot help: the
// standalone fallback has no daemon to hand the write to, so its only correct
// answers are "I own the dir" or "I do not start".
#[cfg(all(test, unix))]
mod standalone_tests {
	use super::*;
	use crate::transport::typed::{bind_kern_listener, BindOutcome};
	use std::sync::Arc;
	use std::time::Duration;

	fn scratch_endpoint(tag: &str) -> Endpoint {
		let dir = std::env::temp_dir().join(format!(
			"kern-standalone-{}-{}-{tag}",
			std::process::id(),
			crate::util::now_ms()
		));
		std::fs::create_dir_all(&dir).expect("scratch dir");
		Endpoint::Unix(dir.join("kern.sock"))
	}

	async fn serving(endpoint: &Endpoint) {
		let BindOutcome::Bound(listener) = bind_kern_listener(endpoint).await.expect("bind") else {
			panic!("scratch endpoint already bound");
		};
		let handler = crate::rpc_kern_rpc_server::KernRpcHandler::new(
			Arc::new(crate::test_support::mcp_server()),
			Arc::new(tokio::sync::Notify::new()),
		);
		tokio::spawn(crate::rpc_kern_rpc_server::serve_kern_rpc_loop(
			listener,
			handler,
			crate::test_support::TEST_TOKEN.to_string(),
		));
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn an_unclaimed_data_dir_is_owned_before_the_graph_is_read() {
		let dir = tempfile::tempdir().unwrap();
		let out = claim_standalone(
			dir.path().to_str().unwrap(),
			&scratch_endpoint("free"),
			&crate::test_support::test_caller(),
			1,
			Duration::ZERO,
		)
		.await;
		match out {
			StandaloneEntry::Own(l) => assert!(l.path().ends_with(crate::lock::LOCK_FILE)),
			_ => panic!("nothing holds the dir — the standalone server is its writer"),
		}
	}

	// The defect this closes: a sibling standalone holds the dir and serves no
	// socket, so nothing but the lock can see it. Booting anyway means two whole
	// graphs in memory and the loser's flush landing last.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_held_dir_with_nothing_serving_refuses_rather_than_writing_beside_it() {
		let dir = tempfile::tempdir().unwrap();
		let d = dir.path().to_str().unwrap();
		let _sibling = crate::lock::acquire(d, "mcp-standalone").expect("sibling claims it");

		let out = claim_standalone(
			d,
			&scratch_endpoint("held"),
			&crate::test_support::test_caller(),
			1,
			Duration::ZERO,
		)
		.await;
		match out {
			StandaloneEntry::Refuse(who) => assert!(
				who.contains("mcp-standalone") && who.contains(&std::process::id().to_string()),
				"the refusal names the writer already there: {who}"
			),
			StandaloneEntry::Own(_) => panic!("became a second whole-graph writer"),
			StandaloneEntry::Attach(_) => panic!("attached to a socket nobody bound"),
		}
	}

	// And the cost of that refusal is bounded: the usual holder is the daemon
	// this process spawned, late to bind. Proxying to it beats dying.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_held_dir_whose_holder_answers_is_proxied_to_instead() {
		let dir = tempfile::tempdir().unwrap();
		let d = dir.path().to_str().unwrap();
		let _daemon = crate::lock::acquire(d, "daemon").expect("the daemon claims it");
		let ep = scratch_endpoint("late");
		serving(&ep).await;

		let out = claim_standalone(
			d,
			&ep,
			&crate::test_support::test_caller(),
			5,
			Duration::from_millis(20),
		)
		.await;
		assert!(
			matches!(out, StandaloneEntry::Attach(_)),
			"a holder that answers gets the traffic, not a refusal"
		);
	}
}

#[cfg(test)]
mod ensure_mcp_tests {
	use super::*;

	#[test]
	fn writes_kern_entry_when_file_absent() {
		let dir = tempfile::tempdir().unwrap();
		ensure_mcp_registered(dir.path());
		let raw = std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
		let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
		assert_eq!(v["mcpServers"]["kern"]["command"], "kern");
		assert_eq!(v["mcpServers"]["kern"]["args"][0], "mcp");
	}

	#[test]
	fn preserves_existing_keys_and_is_idempotent() {
		let dir = tempfile::tempdir().unwrap();
		let mcp = dir.path().join(".mcp.json");
		std::fs::write(&mcp, r#"{"mcpServers":{"other":{"command":"other"}}}"#).unwrap();

		ensure_mcp_registered(dir.path());

		let raw = std::fs::read_to_string(&mcp).unwrap();
		let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
		assert_eq!(v["mcpServers"]["other"]["command"], "other");
		assert_eq!(v["mcpServers"]["kern"]["command"], "kern");

		let before = std::fs::read_to_string(&mcp).unwrap();
		ensure_mcp_registered(dir.path());
		let after = std::fs::read_to_string(&mcp).unwrap();
		assert_eq!(before, after, "idempotent: file unchanged on second call");
	}

	#[test]
	fn does_not_overwrite_existing_custom_entries() {
		let dir = tempfile::tempdir().unwrap();
		let mcp = dir.path().join(".mcp.json");
		std::fs::write(
			&mcp,
			r#"{"mcpServers":{"kern":{"command":"custom","args":["x"]}}}"#,
		)
		.unwrap();

		ensure_mcp_registered(dir.path());

		let raw = std::fs::read_to_string(&mcp).unwrap();
		let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
		assert_eq!(v["mcpServers"]["kern"]["command"], "custom");
	}
}

// Item 81. `extra_capabilities` promises `resources` and `prompts` on the proxy
// path — which is the path an agent gets, since `cmd_mcp` reaches `run_proxy`
// whenever a daemon exists or can be spawned. `ProxyServer` had no
// `handle_method`, so the trait default returned `None` and every advertised
// method came back `-32601`. These drive the real `serve_rw` loop over a real
// `ProxyServer` bound to a real daemon on a real socket, because the defect was
// exactly a missing trait method: a test that called the handler directly would
// have gone on passing while `dispatch` never reached it.
#[cfg(all(test, unix))]
mod proxy_method_tests {
	use super::*;
	use serde_json::{json, Value};

	const SEEDED_ID: &str = "e-proxy-read";
	const SEEDED_TEXT: &str = "only the daemon's graph holds this sentence";

	// A daemon over a scratch socket with one entity in it, and a `ProxyServer`
	// attached to it. The proxy holds no graph — only an RPC client — so
	// anything it returns about `SEEDED_ID` provably crossed the socket.
	async fn proxy_on_a_real_daemon(tag: &str) -> ProxyServer {
		let srv = crate::test_support::mcp_server();
		{
			let mut g = srv.graph.write();
			let mut k = crate::base_types::Kern::new("kx", "");
			let mut e = crate::test_support::entity(SEEDED_ID);
			e.set_text(SEEDED_TEXT.to_string());
			k.entities.insert(SEEDED_ID.into(), e);
			g.kerns.insert("kx".into(), k);
		}
		let ep = crate::test_support::scratch_endpoint(tag);
		crate::test_support::serving(srv, &ep).await;
		let client = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint(
			&ep,
			&crate::test_support::test_caller(),
		)
		.await
		.expect("attach to scratch daemon");
		ProxyServer {
			client: Arc::new(TokioMutex::new(client)),
			auth: crate::test_support::test_caller(),
		}
	}

	// The production loop verbatim: `serve_stdio` is `serve_rw` over the process
	// stdio, and `run_proxy` puts it on a blocking thread because `call_tool`
	// crosses back with `block_in_place`.
	async fn drive(proxy: ProxyServer, frames: Vec<Value>) -> Vec<Value> {
		let mut input = String::new();
		for f in &frames {
			input.push_str(&serde_json::to_string(f).unwrap());
			input.push('\n');
		}
		tokio::task::spawn_blocking(move || {
			let mut reader = std::io::Cursor::new(input.into_bytes());
			let mut out: Vec<u8> = Vec::new();
			crate::transport::serve_rw(&mut reader, &mut out, &proxy).expect("stdio loop");
			String::from_utf8(out)
				.unwrap()
				.lines()
				.map(|l| serde_json::from_str::<Value>(l).expect("one JSON frame per line"))
				.collect()
		})
		.await
		.expect("blocking loop")
	}

	fn method_not_found(v: &Value) -> bool {
		v.pointer("/error/code").and_then(Value::as_i64) == Some(-32601)
	}

	// The whole defect in one assertion: the advertisement and the answers, from
	// the same server, in the same session.
	#[tokio::test(flavor = "multi_thread")]
	async fn every_capability_the_proxy_advertises_is_answered_over_the_stdio_loop() {
		let proxy = proxy_on_a_real_daemon("advertised").await;
		let out = drive(
			proxy,
			vec![
				json!({"id": 1, "method": "initialize", "params": {}}),
				json!({"id": 2, "method": "resources/list"}),
				json!({"id": 3, "method": "prompts/list"}),
				json!({"id": 4, "method": "ping"}),
				json!({"id": 5, "method": "prompts/get",
					"params": {"name": "research", "arguments": {"topic": "kern"}}}),
				json!({"id": 6, "method": "resources/read",
					"params": {"uri": format!("thought://{SEEDED_ID}")}}),
			],
		)
		.await;

		assert_eq!(out.len(), 6, "one response per request: {out:?}");

		let caps = &out[0]["result"]["capabilities"];
		assert!(
			caps.get("resources").is_some() && caps.get("prompts").is_some(),
			"initialize advertises both: {caps}"
		);

		assert_eq!(out[1]["result"]["resources"].as_array().unwrap().len(), 4);
		assert_eq!(out[2]["result"]["prompts"].as_array().unwrap().len(), 1);
		assert_eq!(
			out[3]["result"],
			json!({}),
			"ping answers with an empty result"
		);
		assert_eq!(out[4]["result"]["messages"].as_array().unwrap().len(), 1);

		let contents = out[5]["result"]["contents"].as_array().expect("contents");
		assert_eq!(contents.len(), 1);
		assert_eq!(contents[0]["uri"], format!("thought://{SEEDED_ID}"));
		assert!(
			contents[0]["text"].as_str().unwrap().contains(SEEDED_TEXT),
			"the daemon's own entity text came back: {}",
			contents[0]["text"]
		);

		let refused: Vec<usize> = out
			.iter()
			.enumerate()
			.filter(|(_, v)| method_not_found(v))
			.map(|(i, _)| i)
			.collect();
		assert!(refused.is_empty(), "-32601 on frames {refused:?}: {out:?}");
	}

	// The negative control: `-32601` is still reachable on this exact server, so
	// "no frame carried -32601" is a fact about the five methods, not a dead
	// error channel.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_method_the_proxy_does_not_serve_is_still_method_not_found() {
		let proxy = proxy_on_a_real_daemon("unserved").await;
		let out = drive(
			proxy,
			vec![json!({"id": 1, "method": "resources/subscribe", "params": {}})],
		)
		.await;
		assert_eq!(out.len(), 1);
		assert!(method_not_found(&out[0]), "{:?}", out[0]);
	}

	// The verdict has to survive the `call_tool` hop, which carries only
	// `content` and `isError`. Encoding the error as `isError` alone would
	// deliver `-32000` here, so the exact code is the assertion.
	#[tokio::test(flavor = "multi_thread")]
	async fn an_unknown_resource_uri_keeps_its_code_across_the_call_tool_hop() {
		let proxy = proxy_on_a_real_daemon("unknown-uri").await;
		let out = drive(
			proxy,
			vec![json!({"id": 1, "method": "resources/read",
				"params": {"uri": "kern://local/nope"}})],
		)
		.await;
		assert_eq!(out.len(), 1);
		assert_eq!(
			out[0].pointer("/error/code").and_then(Value::as_i64),
			Some(crate::mcp::ERR_NOT_FOUND as i64),
			"not a generic -32000: {:?}",
			out[0]
		);
		assert!(out[0]["error"]["message"]
			.as_str()
			.unwrap()
			.contains("unknown resource"));
	}

	// `resource_read` is transport, not a tool. If it ever reaches a schema an
	// agent sees two names for one surface.
	#[tokio::test(flavor = "multi_thread")]
	async fn the_resource_read_carrier_is_dispatchable_but_not_listed() {
		let proxy = proxy_on_a_real_daemon("not-listed").await;
		let listed = proxy.tools_list();
		assert_eq!(
			listed.len(),
			crate::mcp::tools::typed_tool_schemas().len(),
			"the carrier added no tool"
		);
		assert!(
			!listed
				.iter()
				.any(|t| t.name == crate::mcp::RESOURCE_READ_TOOL),
			"`{}` is on the agent tool surface",
			crate::mcp::RESOURCE_READ_TOOL
		);
	}

	// The four graphless methods are not two implementations that agree; they
	// are one function. This fails the moment either surface grows a private
	// copy.
	#[tokio::test(flavor = "multi_thread")]
	async fn the_graphless_methods_are_the_standalone_servers_own() {
		use crate::transport::McpServer as _;
		let standalone = crate::test_support::mcp_server();
		let cases = [
			("resources/list", json!({})),
			("prompts/list", json!({})),
			("ping", json!({})),
			(
				"prompts/get",
				json!({"name": "research", "arguments": {"topic": "kern"}}),
			),
		];
		for (method, params) in cases {
			let shared = crate::mcp::handle_graphless_method(method, &params)
				.unwrap_or_else(|| panic!("{method} unserved"))
				.expect(method);
			let direct = standalone
				.handle_method(method, params.clone())
				.unwrap_or_else(|| panic!("{method} unserved by the standalone"))
				.expect(method);
			assert_eq!(shared, direct, "{method} answers differ");
		}
	}
}
