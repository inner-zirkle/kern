//! Tests extracted from commands_ingest_cmd.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn ingest_config_carries_dedup_threshold_from_cfg() {
		let mut cfg = config::Config::default();
		cfg.ingest.dedup_threshold = 0.87;
		let ic = ingest_config(&cfg, None);
		assert_eq!(
			ic.dedup_threshold, 0.87,
			"dedup_threshold comes from the user config"
		);
		assert_eq!(ic.dedup_threshold, 0.87);
		let default_dedup = ingest::Config::default().dedup_threshold;
		assert_ne!(
			0.87, default_dedup,
			"test value differs from the default, so the assertion is meaningful"
		);
	}

	#[test]
	fn ingest_config_carries_the_resolved_retention_deadline() {
		let cfg = config::Config::default();
		assert_eq!(
			ingest_config(&cfg, None).valid_until,
			None,
			"no --retention-secs -> no valid_until"
		);
		let deadline = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
		assert_eq!(
			ingest_config(&cfg, Some(deadline)).valid_until,
			Some(deadline)
		);
	}
}
