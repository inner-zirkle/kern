//! The replicated-entry ledger with per-entry expiry: map plus expiry index
//! mutated in lockstep, so retention sweeps are O(expired), not O(table).

use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use base::base_constants::{LEDGER_ROUTING_TTL, LEDGER_THOUGHT_TTL};

const DEFAULT_LEDGER_CAP: usize = 10_000;

struct Entry {
	addr: String,
	expires: Instant,
}

// INVARIANT: every mutation touches `map` and `by_expiry` in lockstep.
#[derive(Default)]
struct Index {
	map: HashMap<String, Entry>,
	by_expiry: BTreeMap<(Instant, String), ()>,
}

impl Index {
	// Drop the prior expiry-index entry BEFORE the cap check, else it goes stale or spuriously evicts another entry.
	fn insert(&mut self, key: String, addr: String, expires: Instant, cap: usize) {
		if let Some(old) = self.map.remove(&key) {
			self.by_expiry.remove(&(old.expires, key.clone()));
		}
		while self.map.len() >= cap {
			let Some((oldest_exp, oldest_key)) = self.by_expiry.keys().next().cloned() else {
				break;
			};
			self.by_expiry.remove(&(oldest_exp, oldest_key.clone()));
			self.map.remove(&oldest_key);
		}
		self.by_expiry.insert((expires, key.clone()), ());
		self.map.insert(key, Entry { addr, expires });
	}

	fn lookup(&self, key: &str, now: Instant) -> Option<String> {
		self.map.get(key).and_then(|e| live_addr(e, now))
	}
}

pub struct Ledger {
	entities: RwLock<Index>,
	routing: RwLock<Index>,
	max_entries: AtomicUsize,
}

impl Ledger {
	pub fn new() -> Self {
		Self {
			entities: RwLock::new(Index::default()),
			routing: RwLock::new(Index::default()),
			max_entries: AtomicUsize::new(DEFAULT_LEDGER_CAP),
		}
	}

	pub fn set_max_entries(&self, cap: usize) {
		self.max_entries.store(cap.max(1), Ordering::Relaxed);
	}

	fn cap(&self) -> usize {
		self.max_entries.load(Ordering::Relaxed)
	}

	pub fn put_thought(&self, id: &str, addr: &str) {
		let expires = Instant::now() + LEDGER_THOUGHT_TTL;
		self
			.entities
			.write()
			.insert(id.to_string(), addr.to_string(), expires, self.cap());
	}

	pub fn put_routing(&self, kern_id: &str, addr: &str) {
		let expires = Instant::now() + LEDGER_ROUTING_TTL;
		self
			.routing
			.write()
			.insert(kern_id.to_string(), addr.to_string(), expires, self.cap());
	}

	pub fn lookup_thought(&self, id: &str) -> Option<String> {
		self.entities.read().lookup(id, Instant::now())
	}

	pub fn lookup_routing(&self, kern_id: &str) -> Option<String> {
		self.routing.read().lookup(kern_id, Instant::now())
	}
}

fn live_addr(e: &Entry, now: Instant) -> Option<String> {
	(e.expires > now).then(|| e.addr.clone())
}

impl Default for Ledger {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn routing_put_then_lookup_round_trips() {
		let l = Ledger::new();
		assert_eq!(l.lookup_routing("k1"), None, "unknown kern -> None");
		l.put_routing("k1", "127.0.0.1:7400");
		assert_eq!(l.lookup_routing("k1"), Some("127.0.0.1:7400".to_string()));
	}

	#[test]
	fn thought_put_then_lookup_round_trips() {
		let l = Ledger::new();
		l.put_thought("t1", "127.0.0.1:7401");
		assert_eq!(l.lookup_thought("t1"), Some("127.0.0.1:7401".to_string()));
		assert_eq!(l.lookup_thought("missing"), None);
	}

	#[test]
	fn live_addr_returns_addr_before_expiry_and_none_after() {
		let base = Instant::now();
		let e = Entry {
			addr: "peer".into(),
			expires: base + Duration::from_secs(10),
		};
		assert_eq!(live_addr(&e, base), Some("peer".to_string()));
		assert_eq!(live_addr(&e, base + Duration::from_secs(20)), None);
	}

	#[test]
	fn eviction_holds_map_at_capacity() {
		let l = Ledger::new();
		l.set_max_entries(2);
		l.put_routing("a", "1");
		l.put_routing("b", "2");
		l.put_routing("c", "3");

		let live = ["a", "b", "c"]
			.iter()
			.filter(|k| l.lookup_routing(k).is_some())
			.count();
		assert_eq!(live, 2, "cap=2 holds at most two entries");
		assert_eq!(
			l.lookup_routing("a"),
			None,
			"soonest-expiring entry is evicted"
		);
		assert_eq!(l.lookup_routing("b"), Some("2".to_string()));
		assert_eq!(l.lookup_routing("c"), Some("3".to_string()));
	}

	#[test]
	fn overwriting_a_key_does_not_evict_another_entry() {
		let l = Ledger::new();
		l.set_max_entries(2);
		l.put_routing("a", "1");
		l.put_routing("b", "2");
		l.put_routing("a", "1b");
		assert_eq!(
			l.lookup_routing("b"),
			Some("2".to_string()),
			"overwrite must not evict b"
		);
		assert_eq!(
			l.lookup_routing("a"),
			Some("1b".to_string()),
			"a updated in place"
		);
	}

	#[test]
	fn set_max_entries_floors_at_one() {
		let l = Ledger::new();
		l.set_max_entries(0);
		assert_eq!(l.cap(), 1, "cap is floored at 1 so inserts can still land");
	}
}
