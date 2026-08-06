//! What the intake looks like right now, and why anything is stuck.
//!
//! A delta whose claims fail to ingest is deliberately LEFT in place and retried
//! forever rather than archived — losing a capture is worse than repeating one.
//! That is only an acceptable trade while it is visible, and until this existed
//! the failure reached a tracing warning inside the daemon and nowhere else: a
//! file that never drains looked exactly like a file that had not been picked up
//! yet. The last error is written beside the queue so the CLI can say which.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

// A directory, so `drain_entry`'s `is_file` guard skips it — a sidecar sitting
// in the queue itself would be read back as a delta and ingested.
pub fn errors_dir(intake_dir: &Path) -> PathBuf {
	intake_dir.join("errors")
}

fn error_path(intake_dir: &Path, name: &str) -> PathBuf {
	errors_dir(intake_dir).join(format!("{name}.txt"))
}

pub fn record_failure(intake_dir: &Path, name: &str, message: &str) {
	let dir = errors_dir(intake_dir);
	if std::fs::create_dir_all(&dir).is_err() {
		return;
	}
	let _ = std::fs::write(error_path(intake_dir, name), message);
}

// Called on every successful drain: a stale error beside a file that has since
// succeeded is worse than no error at all.
pub fn clear_failure(intake_dir: &Path, name: &str) {
	let _ = std::fs::remove_file(error_path(intake_dir, name));
}

pub fn last_failure(intake_dir: &Path, name: &str) -> Option<String> {
	std::fs::read_to_string(error_path(intake_dir, name))
		.ok()
		.map(|s| s.trim().to_string())
		.filter(|s| !s.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
	pub name: String,
	pub age: Option<Duration>,
	pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
	pub dir_exists: bool,
	pub pending: Vec<Pending>,
	pub failed: Vec<String>,
	pub done: usize,
}

impl Report {
	pub fn stuck(&self) -> usize {
		self
			.pending
			.iter()
			.filter(|p| p.last_error.is_some())
			.count()
	}
}

fn names_in(dir: &Path) -> Vec<String> {
	let Ok(entries) = std::fs::read_dir(dir) else {
		return Vec::new();
	};
	let mut out: Vec<String> = entries
		.flatten()
		.filter(|e| e.path().is_file())
		.filter_map(|e| e.file_name().into_string().ok())
		.collect();
	out.sort();
	out
}

pub fn scan(intake_dir: &Path, now: SystemTime) -> Report {
	if !intake_dir.is_dir() {
		return Report::default();
	}
	let pending = names_in(intake_dir)
		.into_iter()
		.map(|name| {
			let age = std::fs::metadata(intake_dir.join(&name))
				.and_then(|m| m.modified())
				.ok()
				.and_then(|t| now.duration_since(t).ok());
			let last_error = last_failure(intake_dir, &name);
			Pending {
				name,
				age,
				last_error,
			}
		})
		.collect();
	Report {
		dir_exists: true,
		pending,
		failed: names_in(&intake_dir.join("failed")),
		done: names_in(&intake_dir.join("done")).len(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_recorded_failure_is_readable_and_clearable() {
		let dir = tempfile::tempdir().unwrap();
		let intake = dir.path();

		assert_eq!(last_failure(intake, "a.txt"), None, "nothing recorded yet");
		record_failure(intake, "a.txt", "distill returned prose\n");
		assert_eq!(
			last_failure(intake, "a.txt").as_deref(),
			Some("distill returned prose")
		);

		clear_failure(intake, "a.txt");
		assert_eq!(
			last_failure(intake, "a.txt"),
			None,
			"a success must not leave the old error behind"
		);
	}

	#[test]
	fn the_error_sidecar_lives_in_a_directory_the_drain_skips() {
		let dir = tempfile::tempdir().unwrap();
		record_failure(dir.path(), "a.txt", "boom");
		assert!(
			errors_dir(dir.path()).is_dir(),
			"a sidecar file in the queue itself would be ingested as a delta"
		);
		let report = scan(dir.path(), SystemTime::now());
		assert!(
			report.pending.is_empty(),
			"the errors/ dir must not read as a pending delta: {:?}",
			report.pending
		);
	}

	#[test]
	fn scan_reports_pending_failed_and_done_separately() {
		let dir = tempfile::tempdir().unwrap();
		let intake = dir.path();
		std::fs::create_dir_all(intake.join("failed")).unwrap();
		std::fs::create_dir_all(intake.join("done")).unwrap();
		std::fs::write(intake.join("waiting.txt"), "x").unwrap();
		std::fs::write(intake.join("stuck.txt"), "y").unwrap();
		std::fs::write(intake.join("failed").join("binary.bin"), "z").unwrap();
		std::fs::write(intake.join("done").join("old.txt"), "w").unwrap();
		record_failure(intake, "stuck.txt", "reason model replied prose");

		let r = scan(intake, SystemTime::now());

		assert_eq!(
			r.pending
				.iter()
				.map(|p| p.name.as_str())
				.collect::<Vec<_>>(),
			vec!["stuck.txt", "waiting.txt"]
		);
		assert_eq!(r.failed, vec!["binary.bin".to_string()]);
		assert_eq!(r.done, 1);
		assert_eq!(r.stuck(), 1, "only the one with a recorded error is stuck");
		assert_eq!(
			r.pending
				.iter()
				.find(|p| p.name == "waiting.txt")
				.and_then(|p| p.last_error.clone()),
			None,
			"a fresh delta is pending, not stuck"
		);
	}

	#[test]
	fn an_absent_intake_dir_is_reported_not_invented() {
		let dir = tempfile::tempdir().unwrap();
		let r = scan(&dir.path().join("nope"), SystemTime::now());
		assert!(!r.dir_exists);
		assert!(r.pending.is_empty() && r.failed.is_empty() && r.done == 0);
	}
}
