use crate::transport::kern_rpc::HealthRes;

// Below this, a mismatching daemon is left alone. Two clients built from
// different binaries — `target/debug/kern` and an installed `kern`, both
// reporting the same crate version — would otherwise restart each other on
// every alternation. Thrash costs more than staleness, so the young daemon wins.
pub const MIN_UPTIME_MS: u64 = 15_000;

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
	/// Same build and same config: proxy straight through.
	Fresh,
	/// Identity differs and the daemon is old enough to replace.
	Stale(&'static str),
	/// Mismatched but too young, or the daemon predates the fields. Warn, never
	/// act: an unknown identity is not evidence of a stale one.
	Hold(&'static str),
}

/// Pure decision so the thrash guard and the unknown-identity rule are testable
/// without a daemon. `mine` is this process's identity.
pub fn verdict(daemon: &HealthRes, my_build: &str, my_config: &str) -> Verdict {
	if daemon.build_id.is_empty() && daemon.config_id.is_empty() {
		return Verdict::Hold("daemon predates the identity handshake");
	}
	if my_build.is_empty() {
		return Verdict::Hold("this client cannot read its own executable");
	}
	let build_differs = !daemon.build_id.is_empty() && daemon.build_id != my_build;
	let config_differs = !daemon.config_id.is_empty() && daemon.config_id != my_config;
	if !build_differs && !config_differs {
		return Verdict::Fresh;
	}
	// 0 means the daemon never stamped a start — treat as unknown, not as old.
	if daemon.uptime_ms < MIN_UPTIME_MS {
		return Verdict::Hold("daemon booted too recently to replace");
	}
	match (build_differs, config_differs) {
		(true, true) => Verdict::Stale("binary and config both changed"),
		(true, false) => Verdict::Stale("binary changed"),
		(false, true) => Verdict::Stale("config changed"),
		(false, false) => unreachable!("handled above"),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn daemon(build: &str, config: &str, uptime_ms: u64) -> HealthRes {
		HealthRes {
			ok: true,
			build_id: build.into(),
			config_id: config.into(),
			uptime_ms,
			..Default::default()
		}
	}

	#[test]
	fn identical_identity_is_fresh() {
		let d = daemon("b1", "c1", 60_000);
		assert_eq!(verdict(&d, "b1", "c1"), Verdict::Fresh);
	}

	#[test]
	fn a_changed_binary_is_stale() {
		let d = daemon("old", "c1", 60_000);
		assert!(matches!(verdict(&d, "new", "c1"), Verdict::Stale(_)));
	}

	#[test]
	fn a_changed_config_alone_is_stale() {
		// The 36h dead-endpoint outage: same binary, kern.toml written after boot.
		let d = daemon("b1", "old", 60_000);
		assert_eq!(verdict(&d, "b1", "new"), Verdict::Stale("config changed"));
	}

	#[test]
	fn an_old_daemon_without_the_fields_is_never_restarted() {
		let d = daemon("", "", 0);
		assert!(matches!(verdict(&d, "b1", "c1"), Verdict::Hold(_)));
	}

	#[test]
	fn a_young_daemon_is_held_even_when_it_differs() {
		let d = daemon("other", "c1", MIN_UPTIME_MS - 1);
		assert!(
			matches!(verdict(&d, "mine", "c1"), Verdict::Hold(_)),
			"thrash guard must outrank the mismatch"
		);
	}

	#[test]
	fn unknown_daemon_uptime_holds() {
		let d = daemon("other", "c1", 0);
		assert!(matches!(verdict(&d, "mine", "c1"), Verdict::Hold(_)));
	}

	#[test]
	fn a_client_that_cannot_read_itself_never_restarts() {
		let d = daemon("b1", "c1", 60_000);
		assert!(matches!(verdict(&d, "", "c1"), Verdict::Hold(_)));
	}
}
