//! Tests extracted from profile.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use std::thread::sleep;
	use std::time::Duration;

	#[test]
	fn profiler_records_checkpoints() {
		let mut prof = Profiler::new("test");
		sleep(Duration::from_millis(10));
		prof.checkpoint("stage1");
		sleep(Duration::from_millis(5));
		prof.checkpoint("stage2");
		sleep(Duration::from_millis(5));

		let profile = prof.finish();

		assert_eq!(profile.name, "test");
		assert_eq!(profile.checkpoints.len(), 2);
		assert_eq!(profile.checkpoints[0].label, "stage1");
		assert_eq!(profile.checkpoints[1].label, "stage2");

		assert!(
			profile.checkpoints[0].elapsed_ms >= 8.0,
			"stage1 took {}",
			profile.checkpoints[0].elapsed_ms
		);
		assert!(
			profile.checkpoints[1].elapsed_ms >= 3.0,
			"stage2 took {}",
			profile.checkpoints[1].elapsed_ms
		);
		assert!(profile.total_ms >= 20.0, "total took {}", profile.total_ms);
	}

	#[test]
	fn profile_display_formats_correctly() {
		let prof = Profile {
			name: "test".to_string(),
			checkpoints: vec![
				Checkpoint {
					label: "stage1".to_string(),
					elapsed_ms: 1.5,
				},
				Checkpoint {
					label: "stage2".to_string(),
					elapsed_ms: 2.3,
				},
			],
			total_ms: 3.8,
		};

		let output = prof.to_string();
		assert!(output.contains("test:"), "output should contain name");
		assert!(
			output.contains("stage1=1.5ms"),
			"output should contain stage1"
		);
		assert!(
			output.contains("stage2=2.3ms"),
			"output should contain stage2"
		);
		assert!(
			output.contains("total 3.8ms"),
			"output should contain total"
		);
	}

	#[test]
	fn render_timeline_scales_and_lists_stages() {
		let profiles = vec![
			Profile {
				name: "fast".to_string(),
				checkpoints: vec![],
				total_ms: 10.0,
			},
			Profile {
				name: "slow".to_string(),
				checkpoints: vec![
					Checkpoint {
						label: "a".to_string(),
						elapsed_ms: 60.0,
					},
					Checkpoint {
						label: "b".to_string(),
						elapsed_ms: 40.0,
					},
				],
				total_ms: 100.0,
			},
		];

		let out = render_timeline(&profiles, 20);
		let lines: Vec<&str> = out.lines().collect();
		assert_eq!(lines.len(), 2);
		assert!(lines[0].contains("fast"), "first row names fast op");
		assert!(
			lines[1].contains("a=60.0ms b=40.0ms"),
			"stages listed: {out}"
		);
		let slow_bar: usize = lines[1].chars().filter(|c| "█▓▒░".contains(*c)).count();
		let fast_bar: usize = lines[0].chars().filter(|c| *c == '█').count();
		assert_eq!(slow_bar, 20, "slow bar spans full width: {out}");
		assert_eq!(fast_bar, 2, "fast bar scaled to 10%: {out}");
	}

	#[test]
	fn render_timeline_empty_and_zero() {
		assert_eq!(render_timeline(&[], 20), "");
		let zero = vec![Profile {
			name: "z".to_string(),
			checkpoints: vec![],
			total_ms: 0.0,
		}];
		assert_eq!(render_timeline(&zero, 20), "");
	}

	#[test]
	fn render_timeline_tiny_nonzero_stage_gets_at_least_one_cell() {
		let profiles = vec![Profile {
			name: "p".to_string(),
			checkpoints: vec![
				Checkpoint {
					label: "big".to_string(),
					elapsed_ms: 99.0,
				},
				Checkpoint {
					label: "tiny".to_string(),
					elapsed_ms: 0.4,
				},
			],
			total_ms: 100.0,
		}];
		let out = render_timeline(&profiles, 20);
		assert!(
			out.contains('▓'),
			"tiny non-zero stage must render >=1 cell: {out}"
		);
	}
}
