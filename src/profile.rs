//! Wall-clock phase timing for one operation: mark checkpoints as it runs,
//! `finish` into per-phase durations for the status/debug surfaces.

use std::time::Instant;

/// One named phase and how long it took, in ms since the previous checkpoint.
#[derive(Debug, Clone)]
pub struct Checkpoint {
	pub label: String,
	pub elapsed_ms: f64,
}

/// A finished timing run: ordered phases plus the total.
#[derive(Debug, Clone)]
pub struct Profile {
	pub name: String,
	pub checkpoints: Vec<Checkpoint>,
	pub total_ms: f64,
}

/// Collects checkpoints from construction until [`Profiler::finish`].
pub struct Profiler {
	name: String,
	start: Instant,
	checkpoints: Vec<(String, Instant)>,
}

impl Profiler {
	pub fn new(name: impl Into<String>) -> Self {
		Self {
			name: name.into(),
			start: Instant::now(),
			checkpoints: vec![],
		}
	}

	/// Mark the end of the phase that ran since the previous checkpoint (or start).
	pub fn checkpoint(&mut self, label: impl Into<String>) {
		self.checkpoints.push((label.into(), Instant::now()));
	}

	/// Convert the marks into per-phase durations. Consumes the profiler; the
	/// total is measured here, not at the last checkpoint.
	pub fn finish(self) -> Profile {
		let total = self.start.elapsed().as_secs_f64() * 1000.0;
		let mut checkpoints = Vec::new();

		let mut prev = self.start;
		for (label, t) in self.checkpoints {
			let elapsed = t.duration_since(prev).as_secs_f64() * 1000.0;
			checkpoints.push(Checkpoint {
				label,
				elapsed_ms: elapsed,
			});
			prev = t;
		}

		Profile {
			name: self.name,
			checkpoints,
			total_ms: total,
		}
	}
}

impl std::fmt::Display for Profile {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let stages = self
			.checkpoints
			.iter()
			.map(|c| format!("{}={:.1}ms", c.label, c.elapsed_ms))
			.collect::<Vec<_>>()
			.join(" ");
		write!(
			f,
			"{}: {} [total {:.1}ms]",
			self.name, stages, self.total_ms
		)
	}
}

pub fn render_timeline(profiles: &[Profile], width: usize) -> String {
	const FILLS: [char; 4] = ['█', '▓', '▒', '░'];
	let max = profiles.iter().map(|p| p.total_ms).fold(0.0_f64, f64::max);
	if max <= 0.0 || profiles.is_empty() {
		return String::new();
	}
	let name_w = profiles
		.iter()
		.map(|p| p.name.chars().count())
		.max()
		.unwrap_or(0);

	let mut out = String::new();
	for p in profiles {
		let mut bar = String::new();
		if p.checkpoints.is_empty() {
			let n = ((p.total_ms / max) * width as f64).round() as usize;
			bar.extend(std::iter::repeat_n('█', n.max(1)));
		} else {
			for (i, c) in p.checkpoints.iter().enumerate() {
				// Floor a positive stage to 1 cell (rounding alone hides a small stage); zero stays empty.
				let n = if c.elapsed_ms > 0.0 {
					(((c.elapsed_ms / max) * width as f64).round() as usize).max(1)
				} else {
					0
				};
				bar.extend(std::iter::repeat_n(FILLS[i % FILLS.len()], n));
			}
			if bar.is_empty() {
				bar.push('█');
			}
		}
		out.push_str(&format!(
			"{:<name_w$}  {:>9.1}ms  {}",
			p.name, p.total_ms, bar
		));
		if !p.checkpoints.is_empty() {
			let stages = p
				.checkpoints
				.iter()
				.map(|c| format!("{}={:.1}ms", c.label, c.elapsed_ms))
				.collect::<Vec<_>>()
				.join(" ");
			out.push_str(&format!("  ({stages})"));
		}
		out.push('\n');
	}
	out
}

#[cfg(test)]
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
