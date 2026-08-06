use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::RwLock;

use crate::base::graph::GraphGnn;

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
mod tests {
	use super::*;
	use crate::base::types::Kern;

	fn graph_with(ids: &[(&str, Option<SystemTime>)]) -> GraphGnn {
		let mut g = GraphGnn::new();
		let root_id = g.root.id.clone();
		for (id, last) in ids {
			let mut k = Kern::new(*id, &root_id);
			k.last_access = *last;
			g.kerns.insert((*id).to_string(), k);
		}
		g
	}

	#[test]
	fn is_idle_gates_on_elapsed_timeout_and_clock_skew() {
		let now = SystemTime::now();
		let timeout = Duration::from_secs(300);
		assert!(
			!is_idle(Some(now), now, timeout),
			"just accessed -> resident"
		);
		assert!(
			!is_idle(Some(now - Duration::from_secs(299)), now, timeout),
			"under the timeout -> resident"
		);
		assert!(
			is_idle(Some(now - timeout), now, timeout),
			"exactly the timeout -> idle (>=)"
		);
		assert!(
			!is_idle(Some(now + Duration::from_secs(600)), now, timeout),
			"clock skew (access in the future) never evicts"
		);
		assert!(
			!is_idle(None, now, timeout),
			"unknown last access -> resident; None described every kern at boot, \
			 and sweeping them all was the wiped-store bug"
		);
	}

	#[test]
	fn idle_victims_never_include_the_root_kern() {
		let now = SystemTime::now();
		let timeout = Duration::from_secs(300);
		let mut g = graph_with(&[("old", Some(now - Duration::from_secs(600)))]);
		let root_id = g.root.id.clone();
		if let Some(r) = g.kerns.get_mut(&root_id) {
			r.last_access = Some(now - Duration::from_secs(86_400));
		}

		let victims = idle_victims(&g, now, timeout);
		assert!(
			!victims.contains(&root_id),
			"root is idle for a day and still never a victim, got {victims:?}"
		);
		assert_eq!(victims, vec!["old".to_string()]);
	}

	#[test]
	fn idle_victims_spare_recently_accessed_kerns() {
		let now = SystemTime::now();
		let timeout = Duration::from_secs(300);
		let g = graph_with(&[
			("cold", Some(now - Duration::from_secs(600))),
			("warm", Some(now - Duration::from_secs(10))),
		]);

		let victims = idle_victims(&g, now, timeout);
		assert_eq!(victims, vec!["cold".to_string()], "only the cold kern");
	}

	#[test]
	fn a_storeless_graph_is_never_swept() {
		let now = SystemTime::now();
		let g = graph_with(&[("old", Some(now - Duration::from_secs(600)))]);
		let graph = Arc::new(RwLock::new(g));

		assert_eq!(
			run_idle_sweep(&graph, Duration::from_secs(300)),
			0,
			"no store means unload is irreversible; sweep must refuse"
		);
		assert!(
			graph.read().kerns.contains_key("old"),
			"the idle kern stays resident"
		);

		// Why the sweep keeps its own guard: `unload` reports a storeless refusal
		// as success, so dropping the guard would return 1 for zero work done.
		let mut g = graph.write();
		assert!(
			g.unload("old").is_ok(),
			"unload calls a storeless refusal Ok"
		);
		assert!(
			g.kerns.contains_key("old"),
			"the kern is untouched despite the Ok"
		);
	}

	fn stored_graph(dir: &std::path::Path) -> GraphGnn {
		let mut g = GraphGnn::new();
		g.data_dir = dir.to_string_lossy().into_owned();
		g.set_store(Arc::new(
			crate::base::store::Store::open(&g.data_dir).unwrap(),
		));
		g
	}

	#[test]
	fn an_idle_kern_unloads_and_reloads_transparently_with_its_entities() {
		use crate::base::types::{mk_entity, EntityKind};

		let dir = tempfile::tempdir().unwrap();
		let mut g = stored_graph(dir.path());
		let root_id = g.root.id.clone();

		let mut k = Kern::new("idle", &root_id);
		k.entities.insert(
			"e1".into(),
			mk_entity("e1", "remembered", 0.0, EntityKind::Claim),
		);
		k.last_access = Some(SystemTime::now() - Duration::from_secs(600));
		g.register(k);

		let graph = Arc::new(RwLock::new(g));
		let unloaded = run_idle_sweep(&graph, Duration::from_secs(300));
		assert_eq!(unloaded, 1, "the idle kern is unloaded");

		{
			let g = graph.read();
			assert!(!g.kerns.contains_key("idle"), "no longer resident");
			assert_eq!(g.count(), 2, "unloaded still counts: root + idle");
		}

		let mut g = graph.write();
		let reloaded = g.get("idle").expect("touching an unloaded kern reloads it");
		assert_eq!(
			reloaded.entities.get("e1").map(|e| e.statements.clone()),
			Some(vec!["remembered".to_string()]),
			"entities survive the unload/reload round trip"
		);
	}

	#[test]
	fn the_root_kern_is_never_unloaded_by_a_sweep() {
		let dir = tempfile::tempdir().unwrap();
		let mut g = stored_graph(dir.path());
		let root_id = g.root.id.clone();
		if let Some(r) = g.kerns.get_mut(&root_id) {
			r.last_access = Some(SystemTime::now() - Duration::from_secs(86_400));
		}

		let graph = Arc::new(RwLock::new(g));
		assert_eq!(run_idle_sweep(&graph, Duration::from_secs(1)), 0);
		assert!(
			graph.read().kerns.contains_key(&root_id),
			"root stays resident however long it idles"
		);
	}

	#[test]
	fn a_zero_timeout_disables_the_sweep() {
		let now = SystemTime::now();
		let g = graph_with(&[("old", Some(now - Duration::from_secs(600)))]);
		let graph = Arc::new(RwLock::new(g));
		assert_eq!(run_idle_sweep(&graph, Duration::ZERO), 0);
	}
}
