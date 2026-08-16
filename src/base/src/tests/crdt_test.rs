//! Tests extracted from crdt.rs
#![allow(unused)]
use super::*;

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
