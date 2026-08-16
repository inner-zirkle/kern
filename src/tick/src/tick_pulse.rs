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
#[path = "tests/tick_pulse_test.rs"]
mod tick_pulse_tests;
