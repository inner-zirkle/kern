use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use transport::hub_rpc::{
	HubRpc, HubStatusRes, NodeLite, ResolveReq, ResolveRes, StopRes, UnloadReq, UnloadRes,
};
use transport::typed::{Channel, Endpoint, JsonEnvelopeCodec};

use super::node::{self, NodeHandle};

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
	Ok(crate::config::Config::resolve_root(&canon))
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
					if handle.alive() && node::probe(&handle.root).await {
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
			match node::spawn(&root).await {
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
					None => node::probe(&handle.root).await,
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
				if node::probe(&root).await {
					let mut adopted = NodeHandle {
						root,
						endpoint,
						child: None,
					};
					let err = node::shutdown(&mut adopted).await.err().unwrap_or_default();
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
			match node::shutdown(&mut handle).await {
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
		let idle = node::idle_ms(&root).await.unwrap_or(0);
		if idle == 0 || idle < cutoff_ms {
			continue;
		}
		let lock = root_lock(&handler.spawn_locks, &root).await;
		let _guard = lock.lock().await;
		// Re-check under the root lock: a resolve+tool-call may have landed
		// between the first poll and here.
		let idle = node::idle_ms(&root).await.unwrap_or(0);
		if idle == 0 || idle < cutoff_ms {
			continue;
		}
		let Some(mut handle) = handler.nodes.lock().await.remove(&root) else {
			continue;
		};
		match node::shutdown(&mut handle).await {
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
