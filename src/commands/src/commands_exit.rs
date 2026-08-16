//! The CLI's failure channel: one place that decides what an error looks like,
//! and one flag that decides what the process exits with.
//!
//! Every command body returns `()` — the outcome a caller can branch on is the
//! exit status, and before this a failed `kern get`, `kern repair` or `kern
//! import` printed its complaint and still exited 0. Anything scripting kern
//! had to grep stderr to tell a miss from a hit. `fail` is the only way to
//! report an error, so the prefix is uniform and the status follows from it.

use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};

static FAILED: AtomicBool = AtomicBool::new(false);

/// Report a failure as `kern <command>: <message>` on stderr and mark the run
/// failed. Never prints to stdout: stdout is the CLI's answer channel.
pub(crate) fn fail(command: &str, message: impl Display) {
	eprintln!("kern {command}: {message}");
	FAILED.store(true, Ordering::Relaxed);
}

/// The "and here is what to do about it" line under a `fail`. Indented, and it
/// does not re-mark the run — the failure it explains already did.
pub(crate) fn hint(message: impl Display) {
	eprintln!("  {message}");
}

/// True once any command has reported a failure. `main` turns this into a
/// non-zero exit; nothing else reads it.
pub fn failed() -> bool {
	FAILED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
	use super::*;

	// Runs in its own process under nextest, so the global is this test's alone.
	#[test]
	fn a_reported_failure_is_what_the_exit_status_reads() {
		assert!(!failed(), "a run that reported nothing has not failed");
		fail("get", "no thought with id 'ghost'");
		assert!(failed(), "the report is what main exits non-zero on");
	}
}
