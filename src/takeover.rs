//! Hot reload: the daemon watches its own binary and, when a new build lands
//! at the same path, hands its listening socket to a freshly spawned successor
//! and exits. The successor inherits the socket as fd 0, so connections that
//! arrive during its boot queue in the kernel backlog instead of failing —
//! clients see a pause, never a refused connect.
//!
//! Unix only. Windows named pipes cannot cross a process boundary this way;
//! there the client-side auto-restart covers staleness instead.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::identity::{path_fingerprint, self_exe_path};

/// Set in the successor's environment. Its presence means "fd 0 is the bound
/// kern.sock listener — adopt it, do not bind, do not self-heal the store
/// (the predecessor still holds the env for the last few milliseconds)."
pub const TAKEOVER_ENV: &str = "KERN_TAKEOVER";

pub fn is_takeover_boot() -> bool {
	std::env::var_os(TAKEOVER_ENV).is_some()
}

/// Watches the binary the daemon was launched from. Two consecutive polls must
/// agree on the *same changed* fingerprint before triggering, so a partially
/// written file mid-link never fires a takeover into a torn binary.
pub fn spawn_self_watch(
	shutdown: Arc<tokio::sync::Notify>,
	takeover: Arc<AtomicBool>,
	poll_secs: u64,
) {
	let Some(path) = self_exe_path() else {
		tracing::warn!(target: "kern.reload", "cannot resolve own executable — hot reload off");
		return;
	};
	let Some(boot_fp) = path_fingerprint(&path) else {
		tracing::warn!(target: "kern.reload", "cannot fingerprint own executable — hot reload off");
		return;
	};
	let poll = std::time::Duration::from_secs(poll_secs.max(1));
	tokio::spawn(async move {
		let mut pending: Option<String> = None;
		loop {
			tokio::time::sleep(poll).await;
			// None = file absent or unreadable, i.e. mid-replace. Skip; the next
			// poll sees the finished file.
			let Some(fp) = path_fingerprint(&path) else {
				pending = None;
				continue;
			};
			if fp == boot_fp {
				pending = None;
				continue;
			}
			match &pending {
				Some(prev) if *prev == fp => {
					tracing::info!(
						target: "kern.reload",
						exe = %path.display(),
						"new binary detected — handing over"
					);
					takeover.store(true, Ordering::SeqCst);
					shutdown.notify_one();
					return;
				}
				_ => pending = Some(fp),
			}
		}
	});
}

/// Spawns the successor with the listener as its stdin (fd 0). Stdio slots are
/// dup2'd by the runtime, which clears close-on-exec — no fcntl, no libc dep.
/// stdout/stderr are inherited so the successor keeps logging to the same
/// destination the operator pointed the daemon at.
///
/// Called after the final guarded flush; the caller exits with
/// `process::exit(0)` immediately after, deliberately skipping Drop impls —
/// `LocalListener`'s Drop unlinks the socket path, which would orphan the very
/// fd the successor just inherited.
#[cfg(unix)]
pub fn spawn_successor(listener_fd: std::os::fd::OwnedFd) -> Result<(), String> {
	use std::process::{Command, Stdio};

	let exe = self_exe_path().ok_or("cannot resolve own executable")?;
	Command::new(&exe)
		.arg("--daemon")
		.env(TAKEOVER_ENV, "1")
		.stdin(Stdio::from(listener_fd))
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit())
		.spawn()
		.map_err(|e| format!("spawn {}: {e}", exe.display()))?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn takeover_env_gate_reads_the_environment() {
		// Only assert the negative here: the positive would mutate global env,
		// which races other tests in the same binary.
		if std::env::var_os(TAKEOVER_ENV).is_none() {
			assert!(!is_takeover_boot());
		}
	}

	#[tokio::test]
	async fn self_watch_does_not_fire_on_an_unchanged_binary() {
		let shutdown = Arc::new(tokio::sync::Notify::new());
		let takeover = Arc::new(AtomicBool::new(false));
		spawn_self_watch(shutdown.clone(), takeover.clone(), 1);
		tokio::time::sleep(std::time::Duration::from_millis(2300)).await;
		assert!(
			!takeover.load(Ordering::SeqCst),
			"binary did not change; takeover must not trigger"
		);
	}
}
