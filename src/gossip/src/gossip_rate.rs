//! Per-origin request budget for unauthenticated gossip.
//!
//! `Question` answers a peer's arbitrary embedding vector with a yes/no on
//! whether we hold something above the resolve threshold. That is a membership
//! oracle: content existence is extractable one probe at a time without the
//! content ever being sent. Unlimited, it is extractable in bulk.
//!
//! A budget slows extraction; it does not close the oracle. Closing it needs an
//! authenticated peer identity to refuse on (ROADMAP item 33) — until then the
//! honest description is "expensive", not "prevented".

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// A real peer resolves a handful of questions a minute; a prober wants thousands.
pub const GOSSIP_QUESTION_PER_MIN: u32 = 30;
// Bound on the per-origin table itself.
pub const GOSSIP_RATE_MAX_ORIGINS: usize = 1024;

struct Bucket {
	window_start: Instant,
	count: u32,
}

pub struct RateLimiter {
	inner: Mutex<HashMap<String, Bucket>>,
	per_window: u32,
	window: Duration,
	// Origins are attacker-chosen strings, so the table itself is a memory target.
	max_keys: usize,
	refused: AtomicU64,
}

impl RateLimiter {
	pub fn new(per_window: u32, window: Duration, max_keys: usize) -> Self {
		Self {
			inner: Mutex::new(HashMap::new()),
			per_window,
			window,
			max_keys,
			refused: AtomicU64::new(0),
		}
	}

	pub fn refused(&self) -> u64 {
		self.refused.load(Ordering::Relaxed)
	}

	pub fn allow(&self, origin: &str) -> bool {
		self.allow_at(origin, Instant::now())
	}

	fn allow_at(&self, origin: &str, now: Instant) -> bool {
		let mut map = self.inner.lock();

		if !map.contains_key(origin) && map.len() >= self.max_keys {
			// Expired buckets first — they are free to reclaim and the common case.
			map.retain(|_, b| now.duration_since(b.window_start) < self.window);
			// Still full means a live flood of distinct origins. Evict the oldest
			// rather than refusing outright: refusing would let a spoofing peer lock
			// every real one out, and the table has to stay bounded either way.
			if map.len() >= self.max_keys {
				if let Some(oldest) = map
					.iter()
					.min_by_key(|(_, b)| b.window_start)
					.map(|(k, _)| k.clone())
				{
					map.remove(&oldest);
				}
			}
		}

		let bucket = map.entry(origin.to_string()).or_insert(Bucket {
			window_start: now,
			count: 0,
		});
		if now.duration_since(bucket.window_start) >= self.window {
			bucket.window_start = now;
			bucket.count = 0;
		}
		if bucket.count >= self.per_window {
			drop(map);
			self.refused.fetch_add(1, Ordering::Relaxed);
			return false;
		}
		bucket.count += 1;
		true
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn limiter() -> RateLimiter {
		RateLimiter::new(3, Duration::from_secs(60), 4)
	}

	#[test]
	fn a_peer_spends_its_budget_then_is_refused() {
		let r = limiter();
		let t0 = Instant::now();
		for i in 0..3 {
			assert!(r.allow_at("peer-a", t0), "probe {i} is inside the budget");
		}
		assert!(!r.allow_at("peer-a", t0), "the fourth probe is refused");
		assert_eq!(r.refused(), 1, "and counted");
	}

	#[test]
	fn the_budget_refills_on_the_next_window() {
		let r = limiter();
		let t0 = Instant::now();
		for _ in 0..3 {
			r.allow_at("peer-a", t0);
		}
		assert!(!r.allow_at("peer-a", t0));
		let later = t0 + Duration::from_secs(61);
		assert!(
			r.allow_at("peer-a", later),
			"a slow prober is a legitimate peer, not an attacker"
		);
	}

	#[test]
	fn one_peers_flood_does_not_spend_anothers_budget() {
		let r = limiter();
		let t0 = Instant::now();
		for _ in 0..5 {
			r.allow_at("noisy", t0);
		}
		assert!(
			r.allow_at("quiet", t0),
			"budgets are per origin, or one peer silences the mesh"
		);
	}

	#[test]
	fn the_table_stays_bounded_under_spoofed_origins() {
		let r = limiter();
		let t0 = Instant::now();
		for i in 0..500 {
			r.allow_at(&format!("spoofed-{i}"), t0);
		}
		assert!(
			r.inner.lock().len() <= 4,
			"origins are attacker-chosen, so the table is a memory target: {}",
			r.inner.lock().len()
		);
	}

	#[test]
	fn an_expired_bucket_is_reclaimed_before_anything_live_is_evicted() {
		let r = limiter();
		let t0 = Instant::now();
		for i in 0..4 {
			r.allow_at(&format!("old-{i}"), t0);
		}
		let later = t0 + Duration::from_secs(61);
		assert!(r.allow_at("fresh", later));
		let map = r.inner.lock();
		assert!(
			map.contains_key("fresh") && map.len() <= 4,
			"the stale buckets were free to reclaim"
		);
	}
}
