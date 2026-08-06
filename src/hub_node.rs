use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::transport::kern_rpc::KernRpcClient;
use crate::transport::typed::{Endpoint, JsonEnvelopeCodec};

use crate::base::identity::strip_deleted_marker;

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
	match crate::config::Config::load(root) {
		Ok(cfg) => cfg.log_dir(),
		Err(_) => crate::config::Config::default_in(root).log_dir(),
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
	let (out, err) = crate::config_detached_log::stdio(&node_log_dir(root), "--daemon");
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
