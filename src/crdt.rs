//! The two CRDT primitives federation converges on: a per-replica [`GCounter`]
//! for counts that only grow, and [`lww_wins`] — last-writer-wins ordered by
//! `(lamport, replica id)` — for everything that overwrites. Both are
//! commutative, so gossip delivery order cannot change the merged result.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GCounter {
	slots: BTreeMap<String, u64>,
}

impl GCounter {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn increment(&mut self, replica: &str, by: u64) {
		if by == 0 {
			return;
		}
		*self.slots.entry(replica.to_string()).or_insert(0) += by;
	}

	pub fn value(&self) -> u64 {
		self.slots.values().sum()
	}

	pub fn value_i32(&self) -> i32 {
		self.value().min(i32::MAX as u64) as i32
	}

	pub fn merge(&mut self, other: &GCounter) -> bool {
		let mut changed = false;
		for (k, &v) in &other.slots {
			let cur = self.slots.get(k).copied().unwrap_or(0);
			if v > cur {
				self.slots.insert(k.clone(), v);
				changed = true;
			}
		}
		changed
	}

	pub fn slots(&self) -> &BTreeMap<String, u64> {
		&self.slots
	}
}

// Total order over concurrent writes: higher lamport wins, producer id breaks ties.
// Ties on both are a no-op, which is what makes repeated delivery idempotent.
pub fn lww_wins(remote: (u64, &str), local: (u64, &str)) -> bool {
	remote > local
}

#[cfg(test)]
mod tests {
	use super::*;

	fn slot(replica: &str, value: u64) -> GCounter {
		let mut g = GCounter::new();
		g.increment(replica, value);
		g
	}

	// The four hand-rolled call sites this helper replaced all compared the raw
	// `(lamport, producer)` tuple; this pins that exact semantics.
	#[test]
	fn lww_wins_matches_the_tuple_comparison_it_replaced() {
		let cases = [
			(0u64, "", 0u64, ""),
			(1, "r1", 0, "r1"),
			(0, "r1", 1, "r1"),
			(5, "r2", 5, "r1"),
			(5, "r1", 5, "r2"),
			(5, "r1", 5, "r1"),
			(9, "a", 2, "z"),
			(2, "z", 9, "a"),
		];
		for (rl, rp, ll, lp) in cases {
			assert_eq!(
				lww_wins((rl, rp), (ll, lp)),
				(rl, rp) > (ll, lp),
				"({rl},{rp}) vs ({ll},{lp})"
			);
		}
	}

	#[test]
	fn lww_wins_is_irreflexive_so_redelivery_is_a_noop() {
		assert!(!lww_wins((7, "r1"), (7, "r1")));
	}

	#[test]
	fn lww_wins_is_a_total_order_higher_lamport_then_producer() {
		assert!(lww_wins((2, "r1"), (1, "r9")), "lamport dominates producer");
		assert!(
			lww_wins((5, "r2"), (5, "r1")),
			"producer breaks lamport tie"
		);
		assert!(!lww_wins((5, "r1"), (5, "r2")));
	}

	#[test]
	fn merge_is_per_slot_max() {
		let mut a = slot("r1", 5);
		a.merge(&slot("r1", 3));
		assert_eq!(a.value(), 5);
		a.merge(&slot("r1", 9));
		assert_eq!(a.value(), 9);
	}

	#[test]
	fn merge_is_commutative_and_order_independent() {
		let deltas = [slot("r1", 4), slot("r2", 7), slot("r1", 6)];

		let mut a = GCounter::new();
		for d in [&deltas[0], &deltas[1], &deltas[2], &deltas[1]] {
			a.merge(d);
		}

		let mut b = GCounter::new();
		for d in [&deltas[2], &deltas[1], &deltas[0]] {
			b.merge(d);
		}

		assert_eq!(a, b, "merge must be order- and duplicate-independent");
		assert_eq!(a.value(), 6 + 7);
	}

	#[test]
	fn merge_is_idempotent() {
		let mut a = slot("r1", 5);
		let snapshot = a.clone();
		assert!(!a.merge(&slot("r1", 5)), "re-merging same value is a no-op");
		assert_eq!(a, snapshot);
	}
}
