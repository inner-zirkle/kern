use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// A warn on a hot path floods the log until the log is useless. Counters behind
// such a warn stay exact and unconditional; only the printed line is throttled.
pub struct LogThrottle {
	last_secs: AtomicU64,
	interval_secs: u64,
}

impl LogThrottle {
	pub const fn new(interval_secs: u64) -> Self {
		Self {
			last_secs: AtomicU64::new(0),
			interval_secs,
		}
	}

	// The first call always passes, then at most one per interval. Racing callers
	// may both pass — a duplicate line is cheaper than a lock on a hot path.
	pub fn allow(&self) -> bool {
		// 0 means "never fired", so a pre-1970 clock must not read as never.
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0)
			.max(1);
		let last = self.last_secs.load(Ordering::Relaxed);
		if last != 0 && now.saturating_sub(last) < self.interval_secs {
			return false;
		}
		self.last_secs.store(now, Ordering::Relaxed);
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_first_call_passes_and_the_flood_behind_it_does_not() {
		let t = LogThrottle::new(3600);
		assert!(t.allow(), "the first crossing is always reported");
		for _ in 0..1000 {
			assert!(!t.allow(), "every later call inside the window is silent");
		}
	}

	#[test]
	fn a_zero_interval_never_throttles() {
		let t = LogThrottle::new(0);
		assert!(t.allow());
		assert!(t.allow(), "interval 0 disables throttling");
	}
}
