//! Tests extracted from lock.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn a_second_acquire_is_refused_and_names_the_holder() {
		let dir = tempfile::tempdir().unwrap();
		let d = dir.path().to_str().unwrap();

		let first = acquire(d, "daemon").expect("free dir locks");
		let err = acquire(d, "reembed").expect_err("second writer must be refused");
		match err {
			LockError::Held { holder } => {
				let h = holder.expect("the holder identified itself");
				assert!(h.starts_with("daemon "), "names what holds it: {h}");
				assert!(
					h.contains(&std::process::id().to_string()),
					"and its pid: {h}"
				);
			}
			LockError::Io(e) => panic!("expected Held, got io error: {e}"),
		}

		drop(first);
		acquire(d, "reembed").expect("released lock is re-acquirable");
	}

	#[test]
	fn the_lock_file_is_not_the_lock() {
		// A leftover file from a killed process must not look held — the OS
		// released the lock when that process died, and refusing on file
		// existence alone would need manual cleanup after every crash.
		let dir = tempfile::tempdir().unwrap();
		let d = dir.path().to_str().unwrap();
		std::fs::write(lock_path(d), "daemon pid 999999").unwrap();

		assert!(
			holder(d).is_none(),
			"a stale file with no live holder reads as free"
		);
		acquire(d, "reembed").expect("and is acquirable");
	}

	// The failure this exists to prevent, in miniature: a long rewrite holds the
	// dir, a second process boots believing it owns the graph, and the loser's
	// whole-graph flush lands last. The lock must refuse the second one BEFORE
	// it reads anything, since by flush time both have a full graph in hand.
	#[test]
	fn a_rewrite_in_progress_refuses_the_process_that_would_clobber_it() {
		let dir = tempfile::tempdir().unwrap();
		let d = dir.path().to_str().unwrap();

		let rewriting = acquire(d, "reembed").expect("the rewrite claims the dir");
		assert!(
			acquire(d, "daemon").is_err(),
			"a daemon booting mid-rewrite must be refused, not left to flush over it"
		);
		assert!(
			holder(d).unwrap().starts_with("reembed "),
			"and status names the rewrite as the reason"
		);

		drop(rewriting);
		acquire(d, "daemon").expect("once the rewrite lands, the daemon may own the dir");
	}

	#[test]
	fn holder_reports_free_and_taken() {
		let dir = tempfile::tempdir().unwrap();
		let d = dir.path().to_str().unwrap();
		assert_eq!(holder(d), None, "nothing has ever locked it");

		let held = acquire(d, "daemon").unwrap();
		assert!(
			holder(d).unwrap().starts_with("daemon "),
			"a live holder is reported"
		);
		drop(held);
		assert_eq!(holder(d), None, "and its release is observed");
	}
}
