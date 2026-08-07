//! The machine hub: one per machine, it spawns/probes/stops the per-root kern
//! daemons (the process-level plumbing) and serves the hub RPC that lets
//! clients enumerate and reach every daemon without knowing socket paths.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::transport::kern_rpc::KernRpcClient;
use crate::transport::typed::{Endpoint, JsonEnvelopeCodec};

use gossip::identity::strip_deleted_marker;

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

// Both of these take the *root*, not the endpoint. `Endpoint::kern_for` is an
// FNV hash of the path, so the socket name cannot produce the node's token —
// only the root can, via the config that names its data_dir. The endpoint is
// derived here from the same root, so the two can never drift apart.
fn node_caller(root: &Path) -> crate::transport::kern_rpc::AuthReq {
	crate::rpc::caller_at(root)
}

// None = unreachable; Some(0) also means "treat as active" — daemons predating
// the field report 0 and must never be idle-unloaded on a lie.
pub async fn idle_ms(root: &Path) -> Option<u64> {
	let client = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		&Endpoint::kern_for(root),
		&node_caller(root),
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
		&node_caller(root),
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
		&node_caller(&handle.root),
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

use crate::transport::hub_rpc::{
	HubRpc, HubStatusRes, NodeLite, ResolveReq, ResolveRes, StopRes, UnloadReq, UnloadRes,
};
use crate::transport::typed::Channel;
use tokio::sync::Mutex;

const REAP_INTERVAL_SECS: u64 = 30;

type Nodes = Arc<Mutex<HashMap<PathBuf, NodeHandle>>>;
type SpawnLocks = Arc<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>>;

#[derive(Clone)]
pub struct HubRpcHandler {
	nodes: Nodes,
	// Per-root: a cold boot ready-waits ~10s and must not block other roots.
	// Entries are never removed — bounded by distinct roots per machine.
	spawn_locks: SpawnLocks,
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
		Self {
			nodes: Arc::new(Mutex::new(HashMap::new())),
			spawn_locks: Arc::new(Mutex::new(HashMap::new())),
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
		async move {
			let mut map = nodes.lock().await;
			let mut out = Vec::with_capacity(map.len());
			for (root, handle) in map.iter_mut() {
				// Owned children answer via try_wait; adopted nodes (no child
				// handle) only reveal death through their socket.
				let alive = match handle.child {
					Some(_) => handle.alive(),
					None => probe(&handle.root).await,
				};
				out.push(NodeLite {
					root: root.display().to_string(),
					endpoint: handle.endpoint.display(),
					pid: handle.pid(),
					alive,
				});
			}
			HubStatusRes {
				ok: true,
				nodes: out,
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
			if idle_unload_secs == 0 {
				continue;
			}
			idle_pass(&handler, idle_unload_secs * 1000).await;
		}
	});
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
	let mut listener = match crate::transport::typed::bind_kern_listener(&endpoint).await {
		Ok(crate::transport::typed::BindOutcome::Bound(l)) => l,
		Ok(crate::transport::typed::BindOutcome::AlreadyRunning) => {
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
				if let Err(e) = crate::transport::hub_rpc::serve_hub_rpc(channel, handler).await {
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
