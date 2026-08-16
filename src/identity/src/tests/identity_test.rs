//! Tests extracted from identity.rs
#![allow(unused)]
use super::*;

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
		let a = config::Config::default();
		let mut b = config::Config::default();
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
