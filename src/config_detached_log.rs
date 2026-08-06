//! Captured output for a detached child. Two spawners need it — the direct
//! `kern mcp` fallback and the hub's per-root node — and a child whose stdio is
//! `/dev/null` is a child whose failures are unobservable, which is the whole
//! reason this exists.

use std::path::{Path, PathBuf};
use std::process::Stdio;

// One file per spawn arg, so hub and daemon never interleave into one log.
pub fn log_path(log_dir: &Path, arg: &str) -> PathBuf {
	log_dir.join(format!("{}.log", arg.trim_start_matches('-')))
}

fn open(log_dir: &Path, arg: &str) -> std::io::Result<(std::fs::File, std::fs::File)> {
	std::fs::create_dir_all(log_dir)
		.and_then(|()| crate::config::open_private_append(&log_path(log_dir, arg)))
		.and_then(|f| f.try_clone().map(|dup| (f, dup)))
}

/// Append, never truncate: a restart must not erase the log explaining why it
/// restarted. A log we cannot open must not cost us the spawn — fall back to
/// `/dev/null` and say so on the parent's stderr, which is still attached here.
pub fn stdio(log_dir: &Path, arg: &str) -> (Stdio, Stdio) {
	match open(log_dir, arg) {
		Ok((out, err)) => (Stdio::from(out), Stdio::from(err)),
		Err(e) => {
			eprintln!(
				"kern: cannot log to {} ({e}) — the detached child's output is discarded",
				log_path(log_dir, arg).display()
			);
			(Stdio::null(), Stdio::null())
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn one_log_file_per_spawn_arg() {
		let dir = Path::new("/tmp/kern-logs");
		assert_eq!(log_path(dir, "hub"), dir.join("hub.log"));
		assert_eq!(
			log_path(dir, "--daemon"),
			dir.join("daemon.log"),
			"the leading dashes are not part of the name"
		);
	}

	#[test]
	fn opening_creates_the_dir_and_appends_rather_than_truncating() {
		let dir = tempfile::tempdir().unwrap();
		let logs = dir.path().join("nested").join("logs");

		{
			use std::io::Write;
			let (mut out, _) = open(&logs, "hub").expect("first open creates the dir");
			out.write_all(b"first\n").unwrap();
		}
		{
			use std::io::Write;
			let (mut out, _) = open(&logs, "hub").expect("reopen");
			out.write_all(b"second\n").unwrap();
		}

		assert_eq!(
			std::fs::read_to_string(log_path(&logs, "hub")).unwrap(),
			"first\nsecond\n",
			"a reopen must not erase what explains the restart"
		);
	}

	#[test]
	fn an_unopenable_log_is_an_error_the_caller_can_see() {
		let dir = tempfile::tempdir().unwrap();
		let blocked = dir.path().join("not-a-dir");
		std::fs::write(&blocked, "i am a file").unwrap();

		assert!(
			open(&blocked, "hub").is_err(),
			"create_dir_all over an existing file must fail, so the fallback is reachable"
		);
	}
}
