//! Tests extracted from commands_intake_cmd.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use ingest::intake_status::Pending;

	#[test]
	fn ages_read_in_the_largest_unit_that_fits() {
		assert_eq!(human_age(Duration::from_secs(0)), "0s");
		assert_eq!(human_age(Duration::from_secs(59)), "59s");
		assert_eq!(human_age(Duration::from_secs(60)), "1m");
		assert_eq!(human_age(Duration::from_secs(3599)), "59m");
		assert_eq!(human_age(Duration::from_secs(3600)), "1h");
		assert_eq!(human_age(Duration::from_secs(86_400)), "1d");
	}

	#[test]
	fn a_stuck_delta_is_distinguishable_from_one_merely_waiting() {
		let r = Report {
			dir_exists: true,
			pending: vec![
				Pending {
					name: "fresh.txt".into(),
					age: Some(Duration::from_secs(5)),
					last_error: None,
				},
				Pending {
					name: "stuck.txt".into(),
					age: Some(Duration::from_secs(7200)),
					last_error: Some("status=failed embed/transient: refused".into()),
				},
			],
			failed: vec!["blob.bin".into()],
			done: 3,
		};
		assert_eq!(r.stuck(), 1, "only the one carrying an error is stuck");
	}
}
