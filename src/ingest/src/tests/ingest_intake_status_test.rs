//! Tests extracted from ingest_intake_status.rs
#![allow(unused)]
use super::*;

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
