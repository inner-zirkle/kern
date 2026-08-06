use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use crate::base_constants::{GOSSIP_SEEN_SET_CAP, GOSSIP_SEEN_TTL};

// Constant TTL makes expiry monotonic in insertion order, so expired entries sit at
// the deque front — the reclaim loop relies on this.
pub struct SeenSet {
	inner: Mutex<SeenInner>,
}

struct SeenInner {
	live: HashMap<String, Instant>,
	order: VecDeque<(String, Instant)>,
}

impl SeenSet {
	pub fn new() -> Self {
		Self {
			inner: Mutex::new(SeenInner {
				live: HashMap::with_capacity(GOSSIP_SEEN_SET_CAP),
				order: VecDeque::with_capacity(GOSSIP_SEEN_SET_CAP),
			}),
		}
	}

	pub fn add_and_check(&self, id: &str) -> bool {
		self.add_and_check_at(id, Instant::now())
	}

	fn add_and_check_at(&self, id: &str, now: Instant) -> bool {
		let mut inner = self.inner.lock();

		if let Some(&expires) = inner.live.get(id) {
			if expires > now {
				return true;
			}
		}

		while inner.order.front().is_some_and(|(_, exp)| *exp <= now) {
			let (fid, fexp) = inner
				.order
				.pop_front()
				.expect("front checked non-empty above");
			// Skip stale duplicates left by a re-insert after expiry.
			if inner.live.get(&fid) == Some(&fexp) {
				inner.live.remove(&fid);
			}
		}

		while inner.order.len() >= GOSSIP_SEEN_SET_CAP {
			let Some((fid, fexp)) = inner.order.pop_front() else {
				break;
			};
			if inner.live.get(&fid) == Some(&fexp) {
				inner.live.remove(&fid);
			}
		}

		let expires = now + GOSSIP_SEEN_TTL;
		inner.live.insert(id.to_string(), expires);
		inner.order.push_back((id.to_string(), expires));
		false
	}

	#[cfg(test)]
	fn len(&self) -> usize {
		self.inner.lock().live.len()
	}

	#[cfg(test)]
	fn len_order(&self) -> usize {
		self.inner.lock().order.len()
	}
}

impl Default for SeenSet {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn first_sight_is_new_repeat_is_seen() {
		let s = SeenSet::new();
		let t0 = Instant::now();
		assert!(!s.add_and_check_at("a", t0), "first sight is new");
		assert!(
			s.add_and_check_at("a", t0),
			"repeat within TTL is suppressed"
		);
	}

	#[test]
	fn distinct_ids_are_each_new() {
		let s = SeenSet::new();
		let t0 = Instant::now();
		assert!(!s.add_and_check_at("a", t0));
		assert!(!s.add_and_check_at("b", t0));
		assert!(!s.add_and_check_at("c", t0));
	}

	#[test]
	fn entry_expires_after_ttl() {
		let s = SeenSet::new();
		let t0 = Instant::now();
		assert!(!s.add_and_check_at("a", t0));
		assert!(s.add_and_check_at("a", t0 + GOSSIP_SEEN_TTL - Duration::from_millis(1)));
		let past = t0 + GOSSIP_SEEN_TTL + Duration::from_secs(1);
		assert!(!s.add_and_check_at("a", past));
		assert!(s.add_and_check_at("a", past), "re-recorded after expiry");
	}

	#[test]
	fn expired_entries_are_reclaimed_not_accumulated() {
		let s = SeenSet::new();
		let t0 = Instant::now();
		for i in 0..1000 {
			let now = t0 + Duration::from_secs(i);
			s.add_and_check_at(&format!("id{i}"), now);
		}
		let live = s.len();
		assert!(
			live <= (GOSSIP_SEEN_TTL.as_secs() as usize) + 2,
			"expired entries must be reclaimed, got {live} live"
		);
	}

	#[test]
	fn count_is_bounded_under_flood_recent_id_survives() {
		let s = SeenSet::new();
		let t0 = Instant::now();
		for i in 0..(GOSSIP_SEEN_SET_CAP + 500) {
			s.add_and_check_at(&format!("f{i}"), t0);
		}
		assert!(s.len() <= GOSSIP_SEEN_SET_CAP, "count must stay bounded");
		let last = format!("f{}", GOSSIP_SEEN_SET_CAP + 499);
		assert!(
			s.add_and_check_at(&last, t0),
			"recent id must survive the flood"
		);
	}

	#[test]
	fn reinsert_after_expiry_gets_a_fresh_ttl_and_leaves_no_stale_dupe() {
		let s = SeenSet::new();
		let t0 = Instant::now();
		assert!(!s.add_and_check_at("a", t0), "first sight");

		let past = t0 + GOSSIP_SEEN_TTL + Duration::from_secs(1);
		assert!(
			!s.add_and_check_at("a", past),
			"expired id is treated as new again"
		);

		let near = past + GOSSIP_SEEN_TTL - Duration::from_millis(1);
		assert!(
			s.add_and_check_at("a", near),
			"re-inserted id carries a fresh TTL"
		);

		assert_eq!(s.len(), 1, "one live entry for the single id");
		assert_eq!(
			s.len_order(),
			s.len(),
			"order and live lengths agree (no stale dupes)"
		);
	}
}
