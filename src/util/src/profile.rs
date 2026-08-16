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
#[path = "tests/profile_test.rs"]
mod profile_tests;
