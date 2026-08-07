//! Build and config identity: fingerprints of the running binary and resolved
//! config, powering the attach-time staleness check and auto-restart. An
//! unreadable executable yields an EMPTY id — unknown must never read as
//! stale, or an unreadable /proc restarts the daemon on every attach.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use util::{content_hash, now_ms};

// A rebuild unlinks the running binary; /proc/self/exe then reads
// "<path> (deleted)". hub::node strips the same marker for the same reason.
pub(crate) fn strip_deleted_marker(s: &str) -> &str {
	s.strip_suffix(" (deleted)").unwrap_or(s)
}

// (len, mtime), not a content hash: hashing a 187 MB debug binary on every
// client start costs more than the staleness it detects. The path is
// deliberately excluded — `cargo install` hardlinks target/release, and the two
// paths are the same build, so including the path would make them fight.
pub fn path_fingerprint(path: &std::path::Path) -> Option<String> {
	let md = std::fs::metadata(path).ok()?;
	let mtime = md
		.modified()
		.ok()?
		.duration_since(std::time::UNIX_EPOCH)
		.ok()?
		.as_nanos();
	Some(format!("{}-{}", md.len(), mtime))
}

/// The path this process was launched from, with the post-rebuild
/// " (deleted)" marker stripped so it stays pollable and respawnable.
pub fn self_exe_path() -> Option<std::path::PathBuf> {
	let exe = std::env::current_exe().ok()?;
	let shown = exe.to_string_lossy().to_string();
	Some(std::path::PathBuf::from(strip_deleted_marker(&shown)))
}

fn exe_fingerprint() -> Option<String> {
	path_fingerprint(&self_exe_path()?)
}

fn short(s: &str) -> String {
	content_hash(s).chars().take(16).collect()
}

/// Identity of the running binary, stable for the process lifetime. Empty when
/// the executable cannot be read — an unknown build must never look stale, or
/// an unreadable `/proc` would restart the daemon on every attach.
pub fn build_id() -> String {
	static ID: OnceLock<String> = OnceLock::new();
	ID.get_or_init(|| exe_fingerprint().map(|f| short(&f)).unwrap_or_default())
		.clone()
}

/// Identity of the *resolved* config, so an edited `kern.toml` reads as stale
/// even when the binary did not change. Empty when it will not serialize.
pub fn config_id(cfg: &crate::config::Config) -> String {
	serde_json::to_string(cfg)
		.map(|s| short(&s))
		.unwrap_or_default()
}

static STARTED_AT_MS: AtomicU64 = AtomicU64::new(0);

/// Stamps process start. Called once from the daemon boot path; a client that
/// never calls it reports uptime 0.
pub fn mark_start() {
	STARTED_AT_MS.store(now_ms(), Ordering::Relaxed);
}

/// Ms since [`mark_start`], or 0 when it was never called. The restart guard
/// reads 0 as "unknown, do not thrash".
pub fn uptime_ms() -> u64 {
	match STARTED_AT_MS.load(Ordering::Relaxed) {
		0 => 0,
		started => now_ms().saturating_sub(started),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn deleted_marker_is_stripped_only_when_present() {
		assert_eq!(strip_deleted_marker("/bin/kern (deleted)"), "/bin/kern");
		assert_eq!(strip_deleted_marker("/bin/kern"), "/bin/kern");
	}

	#[test]
	fn build_id_is_stable_across_calls() {
		assert_eq!(build_id(), build_id(), "OnceLock must not recompute");
	}

	#[test]
	fn config_id_moves_when_config_moves() {
		let a = crate::config::Config::default();
		let mut b = crate::config::Config::default();
		b.embed.url = "http://elsewhere:11434".into();
		assert_ne!(
			config_id(&a),
			config_id(&b),
			"an edited endpoint must read as a different config"
		);
		assert_eq!(config_id(&a), config_id(&a));
	}

	#[test]
	fn uptime_is_zero_until_marked() {
		// mark_start is process-global; only assert the unmarked contract holds
		// for a reader that never marked, which is the client case.
		if STARTED_AT_MS.load(Ordering::Relaxed) == 0 {
			assert_eq!(uptime_ms(), 0);
		}
	}
}

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
mod takeover_tests {
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
