//! The two CRDT primitives reconcile converges on: a per-replica [`GCounter`]
//! for counts that only grow, and [`lww_wins`] — last-writer-wins ordered by
//! `(lamport, replica id)` — for everything that overwrites. Both are
//! commutative, so the order two graphs meet in cannot change the merged
//! result — which is what makes an external-commit absorb safe to retry.

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
#[path = "tests/crdt_test.rs"]
mod crdt_tests;
