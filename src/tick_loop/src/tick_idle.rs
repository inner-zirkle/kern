//! Idle-kern eviction: kerns unaccessed past the timeout are flushed and
//! unloaded so the resident set tracks use, not history. A never-accessed kern
//! is NOT idle — on a fresh boot that state describes the whole graph.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::RwLock;

use graph::graph::GraphGnn;

pub fn is_idle(last_access: Option<SystemTime>, now: SystemTime, timeout: Duration) -> bool {
	match last_access {
		Some(t) => matches!(now.duration_since(t), Ok(age) if age >= timeout),
		// EVERY kern on a freshly booted daemon is in this state, so treating
		// None as idle unloaded the entire graph on the first sweep — and
		// evict_empty_children then read the unloaded children as dead and
		// deregistered them, orphaning their entities (the wiped-store bug).
		// Unknown is not idle; a kern earns idleness from a real access clock.
		None => false,
	}
}

pub fn idle_victims(g: &GraphGnn, now: SystemTime, timeout: Duration) -> Vec<String> {
	let root_id = &g.root.id;
	g.kerns
		.values()
		.filter(|k| &k.id != root_id && is_idle(k.last_access, now, timeout))
		.map(|k| k.id.clone())
		.collect()
}

pub fn run_idle_sweep(graph: &Arc<RwLock<GraphGnn>>, timeout: Duration) -> usize {
	if timeout.is_zero() {
		return 0;
	}
	let victims = {
		let g = graph.read();
		// `unload` already refuses storelessly, but it reports that refusal as
		// `Ok(())`, which this loop would count as an unload. Skip the sweep so
		// the returned count never claims work that did not happen.
		if g.store().is_none() {
			return 0;
		}
		idle_victims(&g, SystemTime::now(), timeout)
	};

	let mut unloaded = 0usize;
	for id in victims {
		// Re-taken per victim so the sweep never holds the write guard across the whole graph.
		let mut g = graph.write();
		match g.unload(&id) {
			Ok(()) => unloaded += 1,
			Err(e) => tracing::warn!(
				target: "kern.idle",
				kern = %id,
				error = %e,
				"idle unload failed; kern stays resident"
			),
		}
	}
	unloaded
}

#[cfg(test)]
#[path = "tests/tick_idle_test.rs"]
mod tick_idle_tests;
