//! The pulse: a decaying activation wave that fans out from an active kern and
//! enqueues cluster work for every kern it still reaches above threshold, plus
//! the interval gates that piggyback on it (GC sweep, idle sweep, disk
//! consolidation) — single-flighted so concurrent pulses cannot double-fire.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base::base_constants::{
	DISK_CONSOLIDATE_INTERVAL, DISK_CONSOLIDATE_MIN_DELTA, KERN_IDLE_SWEEP_EVERY, PULSE_DECAY,
	PULSE_THRESHOLD, STIGMERGY_GC_INTERVAL,
};
use graph::graph::GraphGnn;

use crate::tick_queue::{task, Queue, TaskKind};

// Unix-seconds of the last GC fan-out; single-flighted by compare_exchange.
static LAST_GC_AT_SECS: AtomicU64 = AtomicU64::new(0);

pub fn pulse(q: &Queue, g: &GraphGnn, kern_id: &str, strength: f64) {
	fan_out_cluster(q, g, kern_id, strength);
	if strength >= PULSE_THRESHOLD {
		maybe_enqueue_stigmergy_gc(q, g);
		maybe_enqueue_reembed(q, g);
		maybe_enqueue_disk_consolidate(q, g);
		maybe_enqueue_idle_sweep(q);
	}
}

// Unix-seconds of the last idle sweep; single-flighted by compare_exchange.
static LAST_IDLE_SWEEP_AT_SECS: AtomicU64 = AtomicU64::new(0);

fn maybe_enqueue_idle_sweep(q: &Queue) {
	if !claim_slot(&LAST_IDLE_SWEEP_AT_SECS, now_secs(), KERN_IDLE_SWEEP_EVERY) {
		return;
	}
	// Graph-global task: a fixed empty key means at most one is ever pending.
	q.enqueue(task(TaskKind::IdleSweep, ""));
}

fn now_secs() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

// Wins the cadence slot for exactly one caller; a fan-out cannot double-fire.
fn claim_slot(cell: &AtomicU64, now_secs: u64, interval: Duration) -> bool {
	let last = cell.load(Ordering::Relaxed);
	should_run_gc(now_secs, last, interval)
		&& cell
			.compare_exchange(last, now_secs, Ordering::AcqRel, Ordering::Relaxed)
			.is_ok()
}

pub fn should_run_gc(now_secs: u64, last_secs: u64, interval: Duration) -> bool {
	if now_secs == 0 || last_secs > now_secs {
		return false;
	}
	now_secs - last_secs >= interval.as_secs()
}

fn maybe_enqueue_stigmergy_gc(q: &Queue, g: &GraphGnn) {
	if !claim_slot(&LAST_GC_AT_SECS, now_secs(), STIGMERGY_GC_INTERVAL) {
		return;
	}
	for kern_id in g.kerns.keys() {
		q.enqueue(task(TaskKind::StigmergyGc, kern_id));
	}
}

// Unix-seconds of the last disk-consolidate fan-out; single-flighted by compare_exchange.
static LAST_CONSOLIDATE_AT_SECS: AtomicU64 = AtomicU64::new(0);

fn maybe_enqueue_disk_consolidate(q: &Queue, g: &GraphGnn) {
	let delta = g.pending_disk_delta_len();
	if delta < DISK_CONSOLIDATE_MIN_DELTA {
		return;
	}
	if !claim_slot(
		&LAST_CONSOLIDATE_AT_SECS,
		now_secs(),
		DISK_CONSOLIDATE_INTERVAL,
	) {
		return;
	}
	// Graph-global task: a fixed empty key means at most one is ever pending.
	q.enqueue(task(TaskKind::DiskConsolidate, ""));
}

fn maybe_enqueue_reembed(q: &Queue, g: &GraphGnn) {
	for (kern_id, k) in g.kerns.iter() {
		let dirty = k.entities.values().any(|e| e.dirty) || k.reasons.values().any(|r| r.dirty);
		if dirty {
			q.enqueue(task(TaskKind::Reembed, kern_id));
		}
	}
}

// The pulse schedules maintenance; it deposits no heat. It used to, and that made
// heat a function of tree position: the deposit recurs every tick, so ANY positive
// amount — the smallest that survives f32 is ~1.6e-7 against the 0.01 cold gate —
// settles at an equilibrium orders of magnitude above the gate and exempts every
// entity within reach from GC forever, used or not. There is no deposit size that
// biases survival without granting that exemption, so the deposit is gone and heat
// is what the vision says it is: a usage signal (ROADMAP item 32).
fn fan_out_cluster(q: &Queue, g: &GraphGnn, kern_id: &str, strength: f64) {
	if strength < PULSE_THRESHOLD {
		return;
	}
	let Some(k) = g.kerns.get(kern_id) else {
		return;
	};
	if !k.entities.is_empty() {
		q.enqueue(task(TaskKind::Cluster, kern_id));
	}
	let reduced = strength * PULSE_DECAY;
	for child_id in &k.children {
		fan_out_cluster(q, g, child_id, reduced);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use base::base_types::{mk_entity, EntityKind, Kern};
	use std::sync::Arc;

	fn cluster_kerns_after_pulse(strength: f64) -> Vec<String> {
		let mut g = GraphGnn::new();
		let mut p = Kern::new("p", "");
		p.children = vec!["c".into()];
		p.entities
			.insert("ep".into(), mk_entity("ep", "x", 0.0, EntityKind::Claim));
		let mut c = Kern::new("c", "p");
		c.entities
			.insert("ec".into(), mk_entity("ec", "y", 0.0, EntityKind::Claim));
		g.kerns.insert("p".into(), p);
		g.kerns.insert("c".into(), c);

		let q = Queue::new(64);
		fan_out_cluster(&q, &g, "p", strength);

		let mut rx = q.take_receiver().unwrap();
		let mut kerns = Vec::new();
		while let Ok(t) = rx.try_recv() {
			if matches!(t.kind, TaskKind::Cluster) {
				kerns.push(t.kern_id.clone());
			}
		}
		kerns
	}

	#[test]
	fn should_run_gc_gates_on_clock_validity_and_elapsed_interval() {
		let iv = Duration::from_secs(100);
		assert!(
			!should_run_gc(0, 0, iv),
			"unreadable clock (now=0) never sweeps"
		);
		assert!(
			!should_run_gc(50, 100, iv),
			"clock skew (last>now) never sweeps"
		);
		assert!(
			!should_run_gc(100, 50, iv),
			"50s elapsed < 100s interval -> no"
		);
		assert!(
			should_run_gc(150, 50, iv),
			"exactly the interval -> yes (>=)"
		);
		assert!(should_run_gc(200, 50, iv), "well past the interval -> yes");
	}

	#[test]
	fn pulse_decays_below_threshold_before_reaching_the_child() {
		let kerns = cluster_kerns_after_pulse(PULSE_THRESHOLD);
		assert!(kerns.contains(&"p".to_string()), "parent clusters");
		assert!(
			!kerns.contains(&"c".to_string()),
			"child is below threshold after one decay"
		);
	}

	#[test]
	fn pulse_reaches_the_child_when_strength_survives_one_decay() {
		let kerns = cluster_kerns_after_pulse(PULSE_THRESHOLD / PULSE_DECAY + 0.01);
		assert!(
			kerns.contains(&"c".to_string()),
			"child clusters when decay keeps it above threshold"
		);
	}

	// The property ROADMAP item 32 is about, driven through the real
	// `pulse` -> `commit_access_ids` -> `run_gc` lifecycle. Fake time: rewinding
	// every stamp by one tick is exactly equivalent to advancing the wall clock,
	// and leaves the code under test untouched. The chain is 8 deep because the
	// pulse reaches 5 levels (1.0 * 0.5^d >= 0.05 for d <= 4) — a shallower tree
	// keeps the boundary outside the graph and the test passes either way. The
	// half-life is compressed to a day so the horizon can clear both gates:
	// COLD_GC_AGE is a fixed 7 days, and a 1.0 access deposit needs 6.64
	// half-lives to fall under the 0.01 cold gate.
	#[test]
	fn at_equal_usage_survival_does_not_depend_on_depth() {
		use crate::retrieval::score::commit_access_ids;
		use crate::tick_stigmergy::run_gc;
		use config::HeatConfig;
		use parking_lot::RwLock;

		const DEPTHS: usize = 8;
		const TICK: Duration = Duration::from_secs(60);
		const TICKS: usize = 9 * 24 * 60;
		let cfg = HeatConfig {
			half_life_secs: 24 * 60 * 60,
			..HeatConfig::default()
		};
		let kid = |d: usize| format!("k{d}");

		let now = SystemTime::now();
		let mut g = GraphGnn::new();
		for d in 0..DEPTHS {
			let mut k = Kern::new(kid(d), if d == 0 { String::new() } else { kid(d - 1) });
			if d + 1 < DEPTHS {
				k.children = vec![kid(d + 1)];
			}
			// Identical stamps and identical starting heat at every depth: one
			// access at t=0, so position is the only thing that varies.
			for id in [format!("used{d}"), format!("unused{d}")] {
				let mut e = mk_entity(&id, "x", 0.0, EntityKind::Claim);
				e.heat = cfg.deposit_access;
				e.heat_updated_at = Some(now);
				e.accessed_at = Some(now);
				e.created_at = Some(now);
				k.entities.insert(id, e);
			}
			g.register(k);
		}

		let used: Vec<String> = (0..DEPTHS).map(|d| format!("used{d}")).collect();
		let graph = Arc::new(RwLock::new(g));
		let q = Queue::new(4096);
		for tick in 0..TICKS {
			{
				let mut g = graph.write();
				for k in g.kerns.values_mut() {
					for e in k.entities.values_mut() {
						for v in [
							&mut e.heat_updated_at,
							&mut e.accessed_at,
							&mut e.created_at,
						]
						.into_iter()
						.flatten()
						{
							*v -= TICK;
						}
					}
				}
				pulse(&q, &g, &kid(0), 1.0);
				// `used` is queried every 6h for the whole run; `unused` never again.
				if tick % (6 * 60) == 0 {
					commit_access_ids(&mut g, &used, &cfg);
				}
			}
			if tick % 60 == 59 {
				for d in 0..DEPTHS {
					run_gc(&graph, &kid(d), &cfg);
				}
			}
		}

		let g = graph.read();
		for d in 0..DEPTHS {
			let e = &g.kerns.get(&kid(d)).expect("kern resident").entities;
			assert!(
				e.contains_key(&format!("used{d}")),
				"depth {d}: a thought queried every 6h was collected — usage must keep it"
			);
			assert!(
				!e.contains_key(&format!("unused{d}")),
				"depth {d}: a thought untouched for 9 days survived while the identical \
				 thought at depth 7 was collected — survival is tracking tree position, \
				 not usage"
			);
		}
	}

	#[test]
	fn claim_slot_lets_exactly_one_caller_through_per_cadence() {
		let cell = AtomicU64::new(0);
		let iv = Duration::from_secs(60);

		assert!(claim_slot(&cell, 1_000, iv), "first call wins the slot");
		assert!(!claim_slot(&cell, 1_000, iv), "same second is gated");
		assert!(!claim_slot(&cell, 1_059, iv), "59s < 60s cadence is gated");
		assert!(claim_slot(&cell, 1_060, iv), "the next cadence wins again");
		assert!(!claim_slot(&cell, 0, iv), "unreadable clock never claims");
	}

	#[test]
	fn concurrent_claims_on_one_cadence_produce_exactly_one_winner() {
		use std::sync::atomic::AtomicUsize;

		static CELL: AtomicU64 = AtomicU64::new(0);
		let winners = Arc::new(AtomicUsize::new(0));
		let iv = Duration::from_secs(60);

		std::thread::scope(|s| {
			for _ in 0..16 {
				let winners = Arc::clone(&winners);
				s.spawn(move || {
					if claim_slot(&CELL, 5_000, iv) {
						winners.fetch_add(1, Ordering::Relaxed);
					}
				});
			}
		});

		assert_eq!(
			winners.load(Ordering::Relaxed),
			1,
			"a 16-way fan-out must not double-fire the sweep"
		);
	}

	#[test]
	fn reembed_is_enqueued_only_for_kerns_with_dirty_content() {
		let mut g = GraphGnn::new();
		let mut dirty = Kern::new("d", "");
		let mut e = mk_entity("e", "x", 0.0, EntityKind::Claim);
		e.dirty = true;
		dirty.entities.insert("e".into(), e);
		let mut clean = Kern::new("c", "");
		clean
			.entities
			.insert("e2".into(), mk_entity("e2", "y", 0.0, EntityKind::Claim));
		g.kerns.insert("d".into(), dirty);
		g.kerns.insert("c".into(), clean);

		let q = Queue::new(64);
		maybe_enqueue_reembed(&q, &g);

		let mut rx = q.take_receiver().unwrap();
		let mut reembed_kerns = Vec::new();
		while let Ok(t) = rx.try_recv() {
			if matches!(t.kind, TaskKind::Reembed) {
				reembed_kerns.push(t.kern_id.clone());
			}
		}
		assert_eq!(
			reembed_kerns,
			vec!["d".to_string()],
			"only the kern with a dirty thought reembeds"
		);
	}
}
