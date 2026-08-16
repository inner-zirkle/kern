//! The machine hub: one per machine, it spawns/probes/stops the per-root kern
//! daemons (the process-level plumbing), keeps the persistent registry of
//! every kern on the machine, and serves the hub RPC that lets clients
//! enumerate, reach, and search across every daemon without knowing socket
//! paths.

pub mod hub_registry;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use transport::kern_rpc::KernRpcClient;
use transport::typed::{Endpoint, JsonEnvelopeCodec};

use identity::strip_deleted_marker;

// Bootstrap loads the whole graph before binding kern.sock, so a big store
// needs a generous ready window.
const READY_RETRIES: u32 = 40;
const READY_DELAY_MS: u64 = 250;

pub struct NodeHandle {
	pub root: PathBuf,
	pub endpoint: Endpoint,
	// None = adopted: a daemon someone else started owns the socket.
	pub child: Option<Child>,
}

impl NodeHandle {
	pub fn pid(&self) -> u32 {
		self.child.as_ref().map(|c| c.id()).unwrap_or(0)
	}

	pub fn alive(&mut self) -> bool {
		match self.child.as_mut() {
			Some(child) => matches!(child.try_wait(), Ok(None)),
			None => true,
		}
	}
}

// None = unreachable; Some(0) also means "treat as active" — daemons predating
// the field report 0 and must never be idle-unloaded on a lie.
pub async fn idle_ms(root: &Path) -> Option<u64> {
	let client = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		&Endpoint::kern_for(root),
		1,
		Duration::from_millis(0),
	)
	.await
	.ok()?;
	let res = client.health().await.ok()?;
	Some(res.idle_ms)
}

pub async fn probe(root: &Path) -> bool {
	KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		&Endpoint::kern_for(root),
		1,
		Duration::from_millis(0),
	)
	.await
	.is_ok()
}

// A rebuild unlinks the running binary; /proc/self/exe then reads
fn self_exe() -> Result<PathBuf, String> {
	let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
	let s = exe.to_string_lossy();
	let stripped = strip_deleted_marker(&s);
	if stripped.len() != s.len() {
		return Ok(PathBuf::from(stripped));
	}
	Ok(exe)
}

fn node_log_dir(root: &Path) -> PathBuf {
	match config::Config::load(root) {
		Ok(cfg) => cfg.log_dir(),
		Err(_) => config::Config::default_in(root).log_dir(),
	}
}

pub async fn spawn(root: &Path) -> Result<NodeHandle, String> {
	let endpoint = Endpoint::kern_for(root);
	if probe(root).await {
		return Ok(NodeHandle {
			root: root.to_path_buf(),
			endpoint,
			child: None,
		});
	}
	let exe = self_exe()?;
	// The hub-first path is the default posture, so THIS is the daemon whose
	// silence hides every fail-open defect. A config we cannot read must not
	// stop the spawn — fall back to the conventional `.kern/data/logs`.
	let (out, err) = config::stdio(&node_log_dir(root), "--daemon");
	let child = Command::new(exe)
		.arg("--daemon")
		.current_dir(root)
		.stdin(Stdio::null())
		.stdout(out)
		.stderr(err)
		.spawn()
		.map_err(|e| format!("spawn node for {}: {e}", root.display()))?;
	for _ in 0..READY_RETRIES {
		if probe(root).await {
			return Ok(NodeHandle {
				root: root.to_path_buf(),
				endpoint,
				child: Some(child),
			});
		}
		tokio::time::sleep(Duration::from_millis(READY_DELAY_MS)).await;
	}
	Err(format!(
		"node for {} never bound {}",
		root.display(),
		endpoint.display()
	))
}

pub async fn shutdown(handle: &mut NodeHandle) -> Result<(), String> {
	if let Ok(client) = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		&handle.endpoint,
		1,
		Duration::from_millis(0),
	)
	.await
	{
		let _ = client.shutdown().await;
	}
	let Some(child) = handle.child.as_mut() else {
		return Ok(());
	};
	for _ in 0..READY_RETRIES {
		if let Ok(Some(_)) = child.try_wait() {
			return Ok(());
		}
		tokio::time::sleep(Duration::from_millis(READY_DELAY_MS)).await;
	}
	// Graceful path stalled — the node's flush already had ~10s; kill beats a
	// zombie holding the socket.
	child.kill().map_err(|e| format!("kill node: {e}"))?;
	let _ = child.wait();
	Ok(())
}

#[cfg(test)]
mod self_exe_tests {
	use super::*;

	#[test]
	fn deleted_marker_is_stripped_only_as_suffix() {
		assert_eq!(
			strip_deleted_marker("/x/kern (deleted)"),
			"/x/kern",
			"rebuilt binary path recovers"
		);
		assert_eq!(strip_deleted_marker("/x/kern"), "/x/kern");
		assert_eq!(
			strip_deleted_marker("/x/kern (deleted)/sub"),
			"/x/kern (deleted)/sub",
			"marker inside the path is a real directory name, not a marker"
		);
	}

	#[test]
	fn self_exe_resolves_to_an_existing_binary() {
		let exe = self_exe().unwrap();
		assert!(exe.exists(), "test runner binary must exist: {exe:?}");
	}
}

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;
use transport::hub_rpc::{
	HubRpc, HubStatusRes, KnownRoot, NodeLite, ResolveReq, ResolveRes, RootErr, SearchHit, SearchReq,
	SearchRes, StopRes, UnloadReq, UnloadRes,
};
use transport::kern_rpc::InvokeReq;
use transport::typed::Channel;

use crate::hub_registry::Registry;

const REAP_INTERVAL_SECS: u64 = 30;
const SEARCH_DEFAULT_K: u64 = 5;

type Nodes = Arc<Mutex<HashMap<PathBuf, NodeHandle>>>;
type SpawnLocks = Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>;

#[derive(Clone)]
pub struct HubRpcHandler {
	nodes: Nodes,
	// Per-root: a cold boot ready-waits ~10s and must not block other roots.
	// Entries are never removed — bounded by distinct roots per machine.
	spawn_locks: SpawnLocks,
	// The persistent memory of every root this machine has resolved — what a
	// restarted hub, `hub status`, and the cross-kern search enumerate.
	registry: Arc<Registry>,
	// Exits the hub loop; nodes stay up (they own their sockets).
	stop: Arc<tokio::sync::Notify>,
}

fn canon(root: &str) -> Result<PathBuf, String> {
	let p = PathBuf::from(root);
	let canon = p
		.canonicalize()
		.map_err(|e| format!("root {}: {e}", p.display()))?;
	if !canon.is_dir() {
		return Err(format!("root {} is not a directory", canon.display()));
	}
	// A booting node re-pins its cwd to the nearest `.kern` ancestor; resolve
	// the same way here or the hub probes a socket the node never binds.
	Ok(config::Config::resolve_root(&canon))
}

async fn root_lock(locks: &SpawnLocks, root: &std::path::Path) -> Arc<Mutex<()>> {
	locks
		.lock()
		.await
		.entry(root.to_path_buf())
		.or_default()
		.clone()
}

impl HubRpcHandler {
	pub fn new() -> Self {
		Self::with_registry(Arc::new(Registry::open_default()))
	}

	pub fn with_registry(registry: Arc<Registry>) -> Self {
		Self {
			nodes: Arc::new(Mutex::new(HashMap::new())),
			spawn_locks: Arc::new(Mutex::new(HashMap::new())),
			registry,
			stop: Arc::new(tokio::sync::Notify::new()),
		}
	}
}

impl Default for HubRpcHandler {
	fn default() -> Self {
		Self::new()
	}
}

impl HubRpc for HubRpcHandler {
	fn resolve(&self, req: ResolveReq) -> impl ::core::future::Future<Output = ResolveRes> + Send {
		let nodes = self.nodes.clone();
		let locks = self.spawn_locks.clone();
		let registry = self.registry.clone();
		async move {
			let root = match canon(&req.root) {
				Ok(p) => p,
				Err(err) => {
					return ResolveRes {
						ok: false,
						err,
						..Default::default()
					}
				}
			};
			let lock = root_lock(&locks, &root).await;
			let _guard = lock.lock().await;
			{
				let mut map = nodes.lock().await;
				if let Some(handle) = map.get_mut(&root) {
					if handle.alive() && probe(&handle.root).await {
						return ResolveRes {
							ok: true,
							endpoint: handle.endpoint.display(),
							spawned: false,
							err: String::new(),
						};
					}
					map.remove(&root);
				}
			}
			// Spawn outside the global map lock — only this root's lock is held.
			match spawn(&root).await {
				Ok(handle) => {
					let endpoint = handle.endpoint.display();
					let spawned = handle.child.is_some();
					registry.record_seen(&root);
					nodes.lock().await.insert(root, handle);
					ResolveRes {
						ok: true,
						endpoint,
						spawned,
						err: String::new(),
					}
				}
				Err(err) => ResolveRes {
					ok: false,
					err,
					..Default::default()
				},
			}
		}
	}

	fn stop(&self) -> impl ::core::future::Future<Output = StopRes> + Send {
		let stop = self.stop.clone();
		async move {
			stop.notify_one();
			StopRes { ok: true }
		}
	}

	fn status(&self) -> impl ::core::future::Future<Output = HubStatusRes> + Send {
		let nodes = self.nodes.clone();
		let registry = self.registry.clone();
		async move {
			let mut loaded: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
			let mut out = Vec::new();
			{
				let mut map = nodes.lock().await;
				for (root, handle) in map.iter_mut() {
					// Owned children answer via try_wait; adopted nodes (no child
					// handle) only reveal death through their socket.
					let alive = match handle.child {
						Some(_) => handle.alive(),
						None => probe(&handle.root).await,
					};
					if alive {
						loaded.insert(root.clone());
					}
					out.push(NodeLite {
						root: root.display().to_string(),
						endpoint: handle.endpoint.display(),
						pid: handle.pid(),
						alive,
					});
				}
			}
			// Every registered root, live and cold, importance-sorted: entity
			// count first (what a search would actually find there), bytes as
			// the tiebreak, path last so the order is total and stable.
			let mut known: Vec<KnownRoot> = registry
				.roots()
				.into_iter()
				.map(|(root, info)| KnownRoot {
					loaded: loaded.contains(&root),
					root: root.display().to_string(),
					entities: info.entities,
					kerns: info.kerns,
					data_bytes: info.data_bytes,
					last_seen_ms: info.last_seen_ms,
				})
				.collect();
			known.sort_by(|a, b| {
				b.entities
					.cmp(&a.entities)
					.then(b.data_bytes.cmp(&a.data_bytes))
					.then(a.root.cmp(&b.root))
			});
			HubStatusRes {
				ok: true,
				nodes: out,
				known,
			}
		}
	}

	// The cross-kern read: fan the query out to every registered root, merge
	// score-descending, and name every root that could not answer. `live_only`
	// restricts to daemons already up; otherwise cold kerns are woken through
	// the same resolve path a client would use (and idle-unload reclaims them).
	fn search(&self, req: SearchReq) -> impl ::core::future::Future<Output = SearchRes> + Send {
		let handler = self.clone();
		async move {
			if req.text.trim().is_empty() {
				return SearchRes {
					ok: false,
					err: "text is required".to_string(),
					..Default::default()
				};
			}
			let k = if req.k == 0 { SEARCH_DEFAULT_K } else { req.k };

			// Ask every root the machine knows: the persistent registry, plus
			// any live node the registry might not have caught up with yet.
			let mut roots: Vec<PathBuf> = handler
				.registry
				.roots()
				.into_iter()
				.map(|(r, _)| r)
				.collect();
			for root in handler.nodes.lock().await.keys() {
				if !roots.contains(root) {
					roots.push(root.clone());
				}
			}
			roots.sort();

			let mut join = tokio::task::JoinSet::new();
			for root in roots {
				let handler = handler.clone();
				let text = req.text.clone();
				let live_only = req.live_only;
				join.spawn(async move {
					let label = root.display().to_string();
					if live_only && !probe(&root).await {
						return Err(RootErr {
							root: label,
							err: "not loaded (live_only)".to_string(),
						});
					}
					// The same resolve path a client uses: adopt or spawn under
					// this root's lock, then one invoke over its socket.
					let res = handler
						.resolve(ResolveReq {
							root: label.clone(),
						})
						.await;
					if !res.ok {
						return Err(RootErr {
							root: label,
							err: res.err,
						});
					}
					let endpoint = Endpoint::parse(&res.endpoint);
					let client = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
						&endpoint,
						1,
						Duration::from_millis(0),
					)
					.await
					.map_err(|e| RootErr {
						root: label.clone(),
						err: format!("connect: {e}"),
					})?;
					let invoked = client
						.invoke(InvokeReq {
							name: "query".to_string(),
							args: serde_json::json!({"text": text, "k": k}),
						})
						.await
						.map_err(|e| RootErr {
							root: label.clone(),
							err: format!("invoke: {e}"),
						})?;
					if !invoked.error.is_empty() {
						return Err(RootErr {
							root: label,
							err: invoked.error,
						});
					}
					let entities = invoked
						.value
						.get("entities")
						.and_then(|v| v.as_array())
						.cloned()
						.unwrap_or_default();
					Ok(
						entities
							.into_iter()
							.map(|entity| SearchHit {
								root: label.clone(),
								entity,
							})
							.collect::<Vec<_>>(),
					)
				});
			}

			let mut hits: Vec<SearchHit> = Vec::new();
			let mut skipped: Vec<RootErr> = Vec::new();
			while let Some(joined) = join.join_next().await {
				match joined {
					Ok(Ok(mut root_hits)) => hits.append(&mut root_hits),
					Ok(Err(miss)) => skipped.push(miss),
					Err(e) => skipped.push(RootErr {
						root: String::new(),
						err: format!("join: {e}"),
					}),
				}
			}
			let score_of = |h: &SearchHit| {
				h.entity
					.get("score")
					.and_then(|v| v.as_f64())
					.unwrap_or(0.0)
			};
			hits.sort_by(|a, b| {
				score_of(b)
					.partial_cmp(&score_of(a))
					.unwrap_or(std::cmp::Ordering::Equal)
					.then_with(|| a.root.cmp(&b.root))
			});
			hits.truncate(k as usize);
			skipped.sort_by(|a, b| a.root.cmp(&b.root));
			SearchRes {
				ok: true,
				hits,
				skipped,
				err: String::new(),
			}
		}
	}

	fn unload(&self, req: UnloadReq) -> impl ::core::future::Future<Output = UnloadRes> + Send {
		let nodes = self.nodes.clone();
		let locks = self.spawn_locks.clone();
		async move {
			let root = match canon(&req.root) {
				Ok(p) => p,
				Err(err) => {
					return UnloadRes {
						ok: false,
						existed: false,
						err,
					}
				}
			};
			let lock = root_lock(&locks, &root).await;
			let _guard = lock.lock().await;
			let handle = nodes.lock().await.remove(&root);
			let Some(mut handle) = handle else {
				// Not tracked — still try the socket so external daemons unload too.
				let endpoint = Endpoint::kern_for(&root);
				if probe(&root).await {
					let mut adopted = NodeHandle {
						root,
						endpoint,
						child: None,
					};
					let err = shutdown(&mut adopted).await.err().unwrap_or_default();
					return UnloadRes {
						ok: err.is_empty(),
						existed: true,
						err,
					};
				}
				return UnloadRes {
					ok: true,
					existed: false,
					err: String::new(),
				};
			};
			match shutdown(&mut handle).await {
				Ok(()) => UnloadRes {
					ok: true,
					existed: true,
					err: String::new(),
				},
				Err(err) => UnloadRes {
					ok: false,
					existed: true,
					err,
				},
			}
		}
	}
}

fn spawn_reaper(handler: HubRpcHandler, idle_unload_secs: u64) {
	// Poll at least as often as the idle threshold, or a short threshold could
	// wait a full default interval past its deadline.
	let reap_secs = if idle_unload_secs > 0 {
		REAP_INTERVAL_SECS.min(idle_unload_secs.max(1))
	} else {
		REAP_INTERVAL_SECS
	};
	tokio::spawn(async move {
		let mut tick = tokio::time::interval(std::time::Duration::from_secs(reap_secs));
		loop {
			tick.tick().await;
			{
				let mut map = handler.nodes.lock().await;
				map.retain(|root, handle| {
					let alive = handle.alive();
					if !alive {
						tracing::info!(target: "kern.hub", root = %root.display(), "reaped dead node");
						return false;
					}
					// An adopted node reports alive() unconditionally, so a node
					// whose project directory was deleted — a finished test's
					// temp dir — would otherwise be tracked until the hub exits.
					if !root.is_dir() {
						tracing::info!(
							target: "kern.hub",
							root = %root.display(),
							"reaped node whose root no longer exists"
						);
						return false;
					}
					true
				});
			}
			// The registry's own revalidation, same cadence: a deleted project
			// stops being enumerated and searched.
			for gone in handler.registry.prune_missing() {
				tracing::info!(target: "kern.hub", root = %gone, "registry dropped a vanished root");
			}
			harvest_stats(&handler).await;
			if idle_unload_secs == 0 {
				continue;
			}
			idle_pass(&handler, idle_unload_secs * 1000).await;
		}
	});
}

// Refresh the registry's size/importance stats from every live node's health
// answer. Cold roots keep the last harvest — the registry reports what its
// daemon last said, never a guess.
async fn harvest_stats(handler: &HubRpcHandler) {
	let live: Vec<PathBuf> = {
		let map = handler.nodes.lock().await;
		map.keys().cloned().collect()
	};
	for root in live {
		let Ok(client) = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
			&Endpoint::kern_for(&root),
			1,
			Duration::from_millis(0),
		)
		.await
		else {
			continue;
		};
		let Ok(health) = client.health().await else {
			continue;
		};
		// data.mdb is the store; its file length is the honest on-disk size
		// (LMDB frees pages internally, so this is an upper bound until a
		// compaction — exactly what `gc` reports too).
		let data_bytes = std::fs::metadata(Path::new(&health.data_dir).join("data.mdb"))
			.map(|m| m.len())
			.unwrap_or(0);
		handler
			.registry
			.record_stats(&root, health.entities, health.kerns, data_bytes);
	}
}

// Only hub-owned nodes (child: Some) are auto-unloaded — a daemon the user
// started by hand is theirs to stop. idle_ms == 0 means a pre-field daemon;
// treated as active, never unloaded on a lie.
async fn idle_pass(handler: &HubRpcHandler, cutoff_ms: u64) {
	let candidates: Vec<PathBuf> = {
		let map = handler.nodes.lock().await;
		map
			.iter()
			.filter(|(_, h)| h.child.is_some())
			.map(|(r, _)| r.clone())
			.collect()
	};
	for root in candidates {
		let idle = idle_ms(&root).await.unwrap_or(0);
		if idle == 0 || idle < cutoff_ms {
			continue;
		}
		let lock = root_lock(&handler.spawn_locks, &root).await;
		let _guard = lock.lock().await;
		// Re-check under the root lock: a resolve+tool-call may have landed
		// between the first poll and here.
		let idle = idle_ms(&root).await.unwrap_or(0);
		if idle == 0 || idle < cutoff_ms {
			continue;
		}
		let Some(mut handle) = handler.nodes.lock().await.remove(&root) else {
			continue;
		};
		match shutdown(&mut handle).await {
			Ok(()) => {
				tracing::info!(
					target: "kern.hub",
					root = %root.display(),
					idle_ms = idle,
					"idle-unloaded node"
				);
			}
			Err(e) => {
				tracing::warn!(target: "kern.hub", root = %root.display(), error = %e, "idle unload");
			}
		}
	}
}

pub async fn run_hub(idle_unload_secs: u64) {
	let endpoint = Endpoint::hub();
	let mut listener = match transport::typed::bind_kern_listener(&endpoint).await {
		Ok(transport::typed::BindOutcome::Bound(l)) => l,
		Ok(transport::typed::BindOutcome::AlreadyRunning) => {
			eprintln!(
				"kern hub: already running at {} — exiting",
				endpoint.display()
			);
			return;
		}
		Err(e) => {
			eprintln!("kern hub: bind {}: {e}", endpoint.display());
			return;
		}
	};
	println!(
		"kern hub listening at {} (ctrl-c to stop)",
		endpoint.display()
	);

	let handler = HubRpcHandler::new();
	spawn_reaper(handler.clone(), idle_unload_secs);

	// Hub exit leaves nodes running on purpose: they own their own sockets and a
	// restarted hub re-adopts them via the probe in resolve().
	let accept = async {
		loop {
			let adapter = match listener.accept().await {
				Ok(a) => a,
				Err(e) => {
					tracing::warn!(target: "kern.hub", error = %e, "accept");
					continue;
				}
			};
			let handler = handler.clone();
			tokio::spawn(async move {
				let channel = Channel::new(adapter, JsonEnvelopeCodec::new());
				if let Err(e) = transport::hub_rpc::serve_hub_rpc(channel, handler).await {
					tracing::warn!(target: "kern.hub", error = %e, "serve loop");
				}
			});
		}
	};
	tokio::select! {
		_ = accept => {}
		_ = handler.stop.notified() => {
			eprintln!("kern hub: stopped via RPC (nodes stay up)");
		}
		_ = tokio::signal::ctrl_c() => {
			eprintln!("kern hub: shutting down (nodes stay up)");
		}
	}
}

#[cfg(test)]
mod canon_tests {
	use super::*;

	#[test]
	fn canon_rejects_a_missing_path() {
		let err = canon("/nonexistent/kern-canon-test").unwrap_err();
		assert!(err.contains("/nonexistent/kern-canon-test"), "{err}");
	}

	#[test]
	fn canon_repins_to_the_nearest_kern_ancestor() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("proj");
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let deep = root.join("src").join("sub");
		std::fs::create_dir_all(&deep).unwrap();
		let resolved = canon(&deep.display().to_string()).unwrap();
		assert_eq!(
			resolved,
			root.canonicalize().unwrap(),
			"a subdir resolve must land on the node's actual socket root"
		);
	}
}
