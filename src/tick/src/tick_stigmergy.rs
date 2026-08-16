//! Heat-driven GC: on the hourly cadence, decay each resident entity's access
//! heat, spill cold victims to the store's cold tier (drop only when no store
//! is bound — counted, not silent), and optionally decay Bayesian evidence
//! toward the Jeffreys prior. Facts are immune; future-stamped entities are
//! skipped and counted as clock skew.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use util::LogThrottle;

use parking_lot::RwLock;

use base::base_constants::{COLD_GC_AGE, COLD_HEAT_THRESHOLD, EVIDENCE_HALF_LIFE_SECS};
use base::base_types::{Entity, EntityKind};
use config::HeatConfig;
use graph::graph::GraphGnn;
use graph::heat;
use graph::reason::remove_entity;

const SKEW_WARN_SECS: u64 = 300;
static CLOCK_SKEW: AtomicU64 = AtomicU64::new(0);
static UNSPILLED_DROPS: AtomicU64 = AtomicU64::new(0);
static SKEW_WARN: LogThrottle = LogThrottle::new(SKEW_WARN_SECS);

// Entities GC could not age because their timestamp is in the future. Nonzero
// means compaction is stalled on a clock problem, not on policy.
pub fn clock_skew_skips() -> u64 {
	CLOCK_SKEW.load(Ordering::Relaxed)
}

// Entities dropped with no cold store to spill into. Unrecoverable by design —
// an in-memory kern has nowhere to put them — so the count is the only trace,
// and the only thing separating that deployment from a durable one.
pub fn unspilled_drops() -> u64 {
	UNSPILLED_DROPS.load(Ordering::Relaxed)
}

fn is_cold_victim(entity: &Entity, now: SystemTime, half_life_secs: u64) -> bool {
	if !entity.is_superseded() && matches!(entity.kind, EntityKind::Fact | EntityKind::Document) {
		return false;
	}
	// Stored heat is only ever refreshed on deposit, so an entity that went cold
	// long ago still carries its last hot value; age it before the comparison.
	// Kind-aware: a distilled preference cools on its Weibull curve (slow), an
	// unlabelled entity on exactly the exponential it always had.
	let heat = heat::decayed_for(entity, now, half_life_secs);
	if (heat as f64) >= COLD_HEAT_THRESHOLD {
		return false;
	}
	let Some(last_touch) = entity.accessed_at.or(entity.created_at) else {
		return false;
	};
	match now.duration_since(last_touch) {
		Ok(age) => age > COLD_GC_AGE,
		// A timestamp in the future means an unreadable or rewound clock. Refusing
		// to reclaim is the safe side — but it is also indefinite: nothing else
		// bounds the hot graph, so a skewed clock stops compaction for as long as
		// it is skewed, and until now said nothing at all (ROADMAP item 7).
		Err(_) => {
			let total = CLOCK_SKEW.fetch_add(1, Ordering::Relaxed) + 1;
			if SKEW_WARN.allow() {
				tracing::warn!(
					target: "kern.gc",
					entity = %entity.id,
					total_skewed = total,
					"entity timestamp is in the future — GC cannot age it, so compaction is \
					 stalled for it; check the system clock (further skew counted, not logged)"
				);
			}
			false
		}
	}
}

// Evidence decay — γ damping of conf_alpha/conf_beta toward the Jeffreys prior
// (1,1). `half_life_secs == 0` is a noop (default-off). Decaying (α-1)/(β-1)
// toward 0 by the heat half-life keeps (1,1) as the floor and never crosses it.
// Local-only mutable state (item 57); superseded entities are skipped.
pub fn decay_evidence(kern: &mut base::base_types::Kern, now: SystemTime, half_life_secs: u64) {
	if half_life_secs == 0 {
		return;
	}
	for t in kern.entities.values_mut() {
		if t.is_superseded() {
			continue;
		}
		t.conf_alpha = 1.0 + heat::decayed(t.conf_alpha - 1.0, t.updated_at, now, half_life_secs);
		t.conf_beta = 1.0 + heat::decayed(t.conf_beta - 1.0, t.updated_at, now, half_life_secs);
		t.refresh_score();
	}
}

pub fn run_gc(graph: &Arc<RwLock<GraphGnn>>, kern_id: &str, heat_cfg: &HeatConfig) {
	let mut g = graph.write();

	let now = SystemTime::now();

	// Evidence decay — tick-based γ damping of conf_alpha/conf_beta toward the
	// Jeffreys prior (1,1). Default-off (EVIDENCE_HALF_LIFE_SECS = 0 → noop,
	// bit-identical to today). Runs on the GC cadence (hourly) per resident
	// non-superseded entity; local-only mutable state (item 57). Decaying (α-1)/(β-1) toward 0 keeps (1,1) as the floor.
	if EVIDENCE_HALF_LIFE_SECS != 0 {
		if let Some(kern) = g.kerns.get_mut(kern_id) {
			decay_evidence(kern, now, EVIDENCE_HALF_LIFE_SECS);
		}
	}

	let kern = match g.kerns.get(kern_id) {
		Some(k) => k,
		None => return,
	};

	let victims: Vec<String> = kern
		.entities
		.values()
		.filter(|t| is_cold_victim(t, now, heat_cfg.half_life_secs))
		.map(|t| t.id.clone())
		.collect();

	if victims.is_empty() {
		return;
	}

	// Spill-before-drop: eviction must never lose data — while a store is bound.
	let kept = match g.store() {
		Some(store) => evict_batched(
			&mut g,
			kern_id,
			&victims,
			|batch| store.cold_put_all(batch),
			|e| store.cold_spill(e).is_ok(),
		),
		// No cold store bound (in-memory kern): dropping IS the intended memory
		// bound, not a bug — there is nowhere to spill to. It is still a real loss,
		// and the spill-before-drop guarantee does not hold here, so count it rather
		// than let an in-memory deployment look like a durable one.
		None => evict_victims(&mut g, kern_id, &victims, |_| {
			UNSPILLED_DROPS.fetch_add(1, Ordering::Relaxed);
			true
		}),
	};
	if kept > 0 {
		tracing::warn!(
			target: "kern.stigmergy",
			kern = %kern_id,
			kept,
			"cold spill failed for {kept} GC victim(s); kept hot, will retry next pass"
		);
	}
}

// One LMDB commit for the whole victim list. A commit is ~9ms and the rest of a
// spill is microseconds, so a sweep's cost was V fsyncs; this makes it one.
//
// A failed batch falls back to the per-victim path, which keeps the failure
// semantics exactly as they were: the bad row stays hot and is retried next
// sweep, every other victim is still collected. All-or-nothing was the
// alternative and it is worse here — cold GC is the only bound on hot-graph
// size, so one un-encodable row would wedge that bound every hour, forever.
// The fallback also absorbs an over-large batch (MDB_TXN_FULL) by finishing the
// sweep slowly instead of not at all.
fn evict_batched(
	g: &mut GraphGnn,
	kern_id: &str,
	victims: &[String],
	spill_all: impl FnOnce(&[Entity]) -> Result<(), store_core::StoreError>,
	spill_one: impl FnMut(&Entity) -> bool,
) -> usize {
	let batch: Vec<Entity> = victims
		.iter()
		.filter_map(|id| entity_of(g, kern_id, id))
		.collect();
	if let Err(err) = spill_all(&batch) {
		tracing::warn!(
			target: "kern.stigmergy",
			kern = %kern_id,
			victims = victims.len(),
			%err,
			"batched cold spill failed; retrying this sweep one victim at a time"
		);
		return evict_victims(g, kern_id, victims, spill_one);
	}
	evict_victims(g, kern_id, victims, |_| true)
}

fn entity_of(g: &GraphGnn, kern_id: &str, id: &str) -> Option<Entity> {
	g.kerns
		.get(kern_id)
		.and_then(|k| k.entities.get(id))
		.cloned()
}

fn evict_victims(
	g: &mut GraphGnn,
	kern_id: &str,
	victims: &[String],
	mut spill: impl FnMut(&Entity) -> bool,
) -> usize {
	let mut kept = 0usize;
	for id in victims {
		if let Some(e) = entity_of(g, kern_id, id) {
			if !spill(&e) {
				kept += 1;
				continue;
			}
		}
		// never forced: GC does not get to punch through fact-immunity.
		remove_entity(g, kern_id, id, false);
	}
	kept
}

#[cfg(test)]
#[path = "tests/tick_stigmergy_test.rs"]
mod tick_stigmergy_tests;
