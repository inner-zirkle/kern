use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use crate::base::log_throttle::LogThrottle;

use parking_lot::RwLock;

use crate::base::constants::{COLD_GC_AGE, COLD_HEAT_THRESHOLD, EVIDENCE_HALF_LIFE_SECS};
use crate::base::graph::GraphGnn;
use crate::base::heat::{self, HeatConfig};
use crate::base::reason::remove_entity;
use crate::base::types::{Entity, EntityKind};

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

// SECURITY: durable-kind immunity only holds for LOCAL kerns. A peer-supplied
// kind=Fact in a phantom kern would otherwise be permanently unreclaimable.
fn is_cold_victim(
	entity: &Entity,
	now: SystemTime,
	half_life_secs: u64,
	kern_is_remote: bool,
) -> bool {
	if !kern_is_remote
		&& !entity.is_superseded()
		&& matches!(entity.kind, EntityKind::Fact | EntityKind::Document)
	{
		return false;
	}
	// Stored heat is only ever refreshed on deposit, so an entity that went cold
	// long ago still carries its last hot value; age it before the comparison.
	let heat = heat::decayed(entity.heat, entity.heat_updated_at, now, half_life_secs);
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
pub fn decay_evidence(kern: &mut crate::base::types::Kern, now: SystemTime, half_life_secs: u64) {
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
	let kern_is_remote = crate::base::merge::is_remote_kern_id(kern_id);

	// Evidence decay — tick-based γ damping of conf_alpha/conf_beta toward the
	// Jeffreys prior (1,1). Default-off (EVIDENCE_HALF_LIFE_SECS = 0 → noop,
	// bit-identical to today). Runs on the GC cadence (hourly) per resident
	// non-superseded entity; local-only mutable state, no gossip/wire change
	// (item 57). Decaying (α-1)/(β-1) toward 0 keeps (1,1) as the floor.
	if EVIDENCE_HALF_LIFE_SECS > 0 {
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
		.filter(|t| is_cold_victim(t, now, heat_cfg.half_life_secs, kern_is_remote))
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
	spill_all: impl FnOnce(&[Entity]) -> Result<(), crate::base::store::StoreError>,
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
mod tests {
	use super::*;
	use crate::base::types::Kern;
	use std::time::Duration;

	const HL: u64 = 3600;

	fn ent(kind: EntityKind, heat: f32, accessed_at: Option<SystemTime>) -> Entity {
		Entity {
			id: "e".into(),
			kind,
			heat,
			accessed_at,
			..Default::default()
		}
	}

	// Five kinds in one kern so a sweep has to separate victims from immune rows
	// rather than collect everything it walks: stale Claim (victim), fresh Claim,
	// stale active Fact (immune), stale superseded Fact (victim), stale Document
	// (immune).
	fn mixed_population(dir: &tempfile::TempDir) -> (GraphGnn, Arc<crate::base::store::Store>) {
		use crate::base::store::Store;
		use crate::base::types::EntityStatus;

		let store = Arc::new(Store::open(&dir.path().to_string_lossy()).unwrap());
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		let mut k = Kern::new("k", "");
		for i in 0..200usize {
			let (kind, stale, superseded) = match i % 5 {
				0 => (EntityKind::Claim, true, false),
				1 => (EntityKind::Claim, false, false),
				2 => (EntityKind::Fact, true, false),
				3 => (EntityKind::Fact, true, true),
				_ => (EntityKind::Document, true, false),
			};
			let mut e = ent(kind, 0.0, Some(if stale { old } else { now }));
			e.id = format!("e{i:03}");
			e.vector = vec![i as f32, 1.0, -1.0].into();
			if superseded {
				e.status = EntityStatus::Superseded;
			}
			k.entities.insert(e.id.clone(), e);
		}
		let mut g = GraphGnn::new();
		g.kerns.insert("k".into(), k);
		g.set_store(store.clone());
		(g, store)
	}

	fn victim_ids(g: &GraphGnn) -> Vec<String> {
		let now = SystemTime::now();
		let mut v: Vec<String> = g.kerns["k"]
			.entities
			.values()
			.filter(|e| is_cold_victim(e, now, HL, false))
			.map(|e| e.id.clone())
			.collect();
		v.sort();
		v
	}

	fn hot_ids(g: &GraphGnn) -> Vec<String> {
		let mut v: Vec<String> = g.kerns["k"].entities.keys().cloned().collect();
		v.sort();
		v
	}

	fn cold_ids(s: &crate::base::store::Store) -> Vec<String> {
		let mut v: Vec<String> = s.cold_all().unwrap().into_iter().map(|e| e.id).collect();
		v.sort();
		v
	}

	// The whole claim of the batched path. A one-victim sweep proves nothing —
	// there both paths are a single commit — so this runs the mixed population,
	// where a divergence in immunity, ordering, or the batch snapshot shows up.
	#[test]
	fn batched_eviction_evicts_exactly_what_the_per_victim_path_evicted() {
		let d_batch = tempfile::tempdir().unwrap();
		let d_each = tempfile::tempdir().unwrap();
		let (mut g_batch, s_batch) = mixed_population(&d_batch);
		let (mut g_each, s_each) = mixed_population(&d_each);

		let victims = victim_ids(&g_batch);
		assert_eq!(
			victims,
			victim_ids(&g_each),
			"precondition: both graphs start identical"
		);
		assert!(
			victims.len() > 1,
			"precondition: a multi-victim sweep, or the two paths cannot be told apart"
		);

		let kept_batch = evict_batched(
			&mut g_batch,
			"k",
			&victims,
			|b| s_batch.cold_put_all(b),
			|e| s_batch.cold_spill(e).is_ok(),
		);
		let kept_each = evict_victims(&mut g_each, "k", &victims, |e| s_each.cold_spill(e).is_ok());

		assert_eq!(kept_batch, kept_each, "same number held back");
		assert_eq!(
			hot_ids(&g_batch),
			hot_ids(&g_each),
			"the batched sweep must leave exactly the survivors the per-victim sweep left"
		);
		assert_eq!(
			cold_ids(&s_batch),
			cold_ids(&s_each),
			"the batched sweep must spill exactly the rows the per-victim sweep spilled"
		);
		assert_eq!(
			cold_ids(&s_batch),
			victims,
			"and those rows are the victims"
		);
	}

	// The failure semantics chosen for the batched path: a failed batch degrades
	// to the per-victim behaviour it replaced, so the bad row stays hot and every
	// other victim is still collected. All-or-nothing was the alternative and it
	// was rejected — cold GC is the only bound on hot-graph size, so one
	// permanently un-encodable row would wedge that bound every hour, forever.
	#[test]
	fn a_failed_batch_falls_back_per_victim_instead_of_holding_the_sweep() {
		let dir = tempfile::tempdir().unwrap();
		let (mut g, _store) = mixed_population(&dir);
		let victims = victim_ids(&g);
		assert!(victims.len() > 1, "precondition: a multi-victim sweep");
		let poison = victims[victims.len() / 2].clone();

		let kept = evict_batched(
			&mut g,
			"k",
			&victims,
			|_| Err(crate::base::store::StoreError::BadVersion(9)),
			|e| e.id != poison,
		);

		assert_eq!(
			kept, 1,
			"only the row that failed its own spill is held back"
		);
		let hot = hot_ids(&g);
		assert!(
			hot.contains(&poison),
			"the row that cannot be spilled stays hot and is retried next sweep"
		);
		for id in victims.iter().filter(|v| **v != poison) {
			assert!(
				!hot.contains(id),
				"a failed batch must not hold the rest of the sweep hot, but {id} survived"
			);
		}
	}

	// Facts are GC-immune while Active, and the batch is a second place that
	// immunity has to hold: a victim list is built once and handed to the store
	// wholesale, so a leak here spills a Fact nothing asked to evict.
	#[test]
	fn a_batched_sweep_never_spills_or_drops_an_active_fact() {
		let dir = tempfile::tempdir().unwrap();
		let (g, store) = mixed_population(&dir);
		let immune: Vec<String> = g.kerns["k"]
			.entities
			.values()
			.filter(|e| matches!(e.kind, EntityKind::Fact | EntityKind::Document) && !e.is_superseded())
			.map(|e| e.id.clone())
			.collect();
		let victims = victim_ids(&g);
		assert!(
			immune.len() > 1 && victims.len() > 1,
			"precondition: many of each"
		);

		let g = Arc::new(RwLock::new(g));
		run_gc(&g, "k", &HeatConfig::default());

		let hot = hot_ids(&g.read());
		let cold = cold_ids(&store);
		for id in &immune {
			assert!(
				hot.contains(id),
				"batched sweep dropped active durable {id}"
			);
			assert!(
				!cold.contains(id),
				"batched sweep spilled active durable {id}"
			);
		}
		for id in &victims {
			assert!(!hot.contains(id), "victim {id} survived the batched sweep");
			assert!(cold.contains(id), "victim {id} was dropped without a spill");
		}
	}

	fn graph_with_cold_claim(id: &str) -> GraphGnn {
		let old = SystemTime::now() - (COLD_GC_AGE + Duration::from_secs(1));
		let mut e = ent(EntityKind::Claim, 0.0, Some(old));
		e.id = id.into();
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert(id.into(), e);
		g.kerns.insert("k".into(), k);
		g
	}

	#[test]
	fn evict_keeps_victim_hot_when_spill_fails() {
		let mut g = graph_with_cold_claim("victim");
		let kept = evict_victims(&mut g, "k", &["victim".to_string()], |_| false);
		assert_eq!(kept, 1, "the failed-spill victim is counted as kept");
		assert!(
			g.kerns.get("k").unwrap().entities.contains_key("victim"),
			"spill failure must NOT drop the thought"
		);
	}

	#[test]
	fn evict_drops_victim_once_spill_succeeds() {
		let mut g = graph_with_cold_claim("victim");
		let kept = evict_victims(&mut g, "k", &["victim".to_string()], |_| true);
		assert_eq!(kept, 0, "a successful spill keeps nothing back");
		assert!(
			!g.kerns.get("k").unwrap().entities.contains_key("victim"),
			"a durably-spilled thought is dropped from the hot tier"
		);
	}

	#[test]
	fn cold_old_claim_is_a_victim() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		assert!(is_cold_victim(
			&ent(EntityKind::Claim, 0.0, Some(old)),
			now,
			HL,
			false
		));
	}

	#[test]
	fn heat_above_threshold_is_preserved_even_when_old() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		let mut hot = ent(EntityKind::Claim, 1e9, Some(old));
		hot.heat_updated_at = Some(now);
		assert!(!is_cold_victim(&hot, now, HL, false));
	}

	#[test]
	fn stale_heat_decays_away_and_stops_shielding_the_entity() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		let mut once_hot = ent(EntityKind::Claim, 1e9, Some(old));
		once_hot.heat_updated_at = Some(old);
		assert!(
			is_cold_victim(&once_hot, now, HL, false),
			"heat last deposited a week ago has decayed below the threshold; \
			 raw stored heat must not grant permanent GC immunity"
		);
	}

	#[test]
	fn durable_kinds_are_never_collected() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		assert!(
			!is_cold_victim(&ent(EntityKind::Fact, 0.0, Some(old)), now, HL, false),
			"Fact preserved"
		);
		assert!(
			!is_cold_victim(&ent(EntityKind::Document, 0.0, Some(old)), now, HL, false),
			"Document preserved"
		);
	}

	// Cold-tier GC is the only bound on graph size; a remote Fact that kept durable
	// immunity would be permanently unreclaimable storage a peer chose to allocate.
	#[test]
	fn remote_durable_kinds_lose_immunity_and_are_reclaimed() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		for kind in [EntityKind::Fact, EntityKind::Document] {
			assert!(
				is_cold_victim(&ent(kind, 0.0, Some(old)), now, HL, true),
				"a cold, stale {kind:?} in a remote kern is reclaimable"
			);
			assert!(
				!is_cold_victim(&ent(kind, 0.0, Some(old)), now, HL, false),
				"the LOCAL {kind:?} keeps its immunity unchanged"
			);
		}
		assert!(
			!is_cold_victim(&ent(EntityKind::Fact, 0.0, Some(now)), now, HL, true),
			"a freshly-touched remote Fact is still spared — remoteness drops immunity, \
			 it does not bypass the staleness and heat gates"
		);
	}

	#[test]
	fn run_gc_reclaims_a_stale_remote_fact() {
		use crate::base::store::Store;

		let dir = tempfile::tempdir().unwrap();
		let store = Arc::new(Store::open(&dir.path().to_string_lossy()).unwrap());

		let old = SystemTime::now() - (COLD_GC_AGE + Duration::from_secs(1));
		let mut pinned = ent(EntityKind::Fact, 0.0, Some(old));
		pinned.id = "pinned".into();

		let mut g = GraphGnn::new();
		let mut k = Kern::new("remote-evilnet-k1", "");
		k.entities.insert("pinned".into(), pinned);
		g.kerns.insert("remote-evilnet-k1".into(), k);
		g.set_store(store.clone());

		let graph = Arc::new(RwLock::new(g));
		run_gc(&graph, "remote-evilnet-k1", &HeatConfig::default());

		assert!(
			!graph
				.read()
				.kerns
				.get("remote-evilnet-k1")
				.unwrap()
				.entities
				.contains_key("pinned"),
			"a peer cannot pin an unreclaimable row by setting kind=Fact"
		);
		assert!(
			store.cold_get("pinned").unwrap().is_some(),
			"spill-before-drop still holds for the remote fact"
		);
	}

	#[test]
	fn superseded_fact_loses_immunity_and_becomes_a_victim() {
		use crate::base::types::EntityStatus;
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		assert!(
			!is_cold_victim(&ent(EntityKind::Fact, 0.0, Some(old)), now, HL, false),
			"active Fact is immune even when stale"
		);
		let mut superseded = ent(EntityKind::Fact, 0.0, Some(old));
		superseded.status = EntityStatus::Superseded;
		assert!(
			is_cold_victim(&superseded, now, HL, false),
			"a superseded (invalidated) Fact is no longer immune"
		);
		let mut fresh_superseded = ent(EntityKind::Fact, 0.0, Some(now));
		fresh_superseded.status = EntityStatus::Superseded;
		assert!(
			!is_cold_victim(&fresh_superseded, now, HL, false),
			"a recently-touched superseded fact is still spared"
		);
	}

	#[test]
	fn run_gc_spills_superseded_fact_to_cold_while_active_fact_stays_immune() {
		use crate::base::store::Store;
		use crate::base::types::EntityStatus;
		use parking_lot::RwLock;
		use std::sync::Arc;

		let dir = tempfile::tempdir().unwrap();
		let store = Arc::new(Store::open(&dir.path().to_string_lossy()).unwrap());

		let old = SystemTime::now() - (COLD_GC_AGE + Duration::from_secs(1));
		let mut invalidated = ent(EntityKind::Fact, 0.0, Some(old));
		invalidated.id = "invalidated".into();
		invalidated.status = EntityStatus::Superseded;
		let mut active_fact = ent(EntityKind::Fact, 0.0, Some(old));
		active_fact.id = "active".into();

		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert("invalidated".into(), invalidated);
		k.entities.insert("active".into(), active_fact);
		g.kerns.insert("k".into(), k);
		g.set_store(store.clone());

		let graph = Arc::new(RwLock::new(g));
		run_gc(&graph, "k", &HeatConfig::default());

		let g = graph.read();
		let entities = &g.kerns.get("k").unwrap().entities;
		assert!(
			!entities.contains_key("invalidated"),
			"the superseded fact is evicted from the hot tier"
		);
		assert!(
			entities.contains_key("active"),
			"the active fact keeps its GC immunity"
		);
		assert!(
			store.cold_get("invalidated").unwrap().is_some(),
			"the invalidated fact was spilled to the cold tier (invalidated != deleted)"
		);
	}

	#[test]
	fn recent_untouched_or_clock_skewed_is_preserved() {
		let now = SystemTime::now();
		assert!(
			!is_cold_victim(&ent(EntityKind::Claim, 0.0, Some(now)), now, HL, false),
			"recently accessed"
		);
		assert!(
			!is_cold_victim(&ent(EntityKind::Claim, 0.0, None), now, HL, false),
			"no timestamps at all"
		);
		let future = now + Duration::from_secs(3600);
		assert!(
			!is_cold_victim(&ent(EntityKind::Claim, 0.0, Some(future)), now, HL, false),
			"clock skew"
		);
	}

	#[test]
	fn created_at_seeds_the_staleness_clock_for_never_accessed_thoughts() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		let mut stale = ent(EntityKind::Claim, 0.0, None);
		stale.created_at = Some(old);
		assert!(
			is_cold_victim(&stale, now, HL, false),
			"old-but-never-queried is a victim"
		);
		let mut fresh = ent(EntityKind::Claim, 0.0, None);
		fresh.created_at = Some(now);
		assert!(
			!is_cold_victim(&fresh, now, HL, false),
			"fresh ingest is preserved"
		);
		let mut touched = ent(EntityKind::Claim, 0.0, Some(now));
		touched.created_at = Some(old);
		assert!(
			!is_cold_victim(&touched, now, HL, false),
			"accessed_at takes precedence over created_at"
		);
	}

	#[test]
	fn run_gc_spills_stale_victim_to_cold_store_and_spares_facts() {
		use crate::base::store::Store;
		use parking_lot::RwLock;
		use std::sync::Arc;

		let dir = tempfile::tempdir().unwrap();
		let store = Arc::new(Store::open(&dir.path().to_string_lossy()).unwrap());

		let old = SystemTime::now() - (COLD_GC_AGE + Duration::from_secs(1));
		let mut victim = ent(EntityKind::Claim, 0.0, Some(old));
		victim.id = "victim".into();
		let mut fact = ent(EntityKind::Fact, 0.0, Some(old));
		fact.id = "fact".into();

		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert("victim".into(), victim);
		k.entities.insert("fact".into(), fact);
		g.kerns.insert("k".into(), k);
		g.set_store(store.clone());

		let graph = Arc::new(RwLock::new(g));
		run_gc(&graph, "k", &HeatConfig::default());

		let g = graph.read();
		let entities = &g.kerns.get("k").unwrap().entities;
		assert!(
			!entities.contains_key("victim"),
			"stale cold claim is evicted from the hot tier"
		);
		assert!(
			entities.contains_key("fact"),
			"Facts are immune to cold GC even when stale"
		);
		let spilled = store.cold_get("victim").unwrap();
		assert!(
			spilled.is_some(),
			"the victim was spilled to the cold tier before the hot drop"
		);
		assert!(
			store.cold_get("fact").unwrap().is_none(),
			"the immune fact was never spilled"
		);
	}
	#[test]
	fn a_future_timestamp_is_not_reclaimed_and_is_counted() {
		// A rewound or unreadable clock makes every entity look untouchable, and
		// nothing else bounds the hot graph — so refusing to reclaim is right, and
		// refusing silently is the defect.
		let future = SystemTime::now() + Duration::from_secs(3600);
		let e = ent(EntityKind::Claim, 0.0, Some(future));

		let before = clock_skew_skips();
		let victim = is_cold_victim(
			&e,
			SystemTime::now(),
			HeatConfig::default().half_life_secs,
			false,
		);

		assert!(!victim, "a future timestamp must never be reclaimed");
		assert_eq!(
			clock_skew_skips(),
			before + 1,
			"and the stall must be countable, not silent"
		);
	}

	#[test]
	fn a_normal_old_entity_is_reclaimed_without_counting_skew() {
		let old = SystemTime::now() - (COLD_GC_AGE + Duration::from_secs(1));
		let e = ent(EntityKind::Claim, 0.0, Some(old));

		let before = clock_skew_skips();
		let victim = is_cold_victim(
			&e,
			SystemTime::now(),
			HeatConfig::default().half_life_secs,
			false,
		);

		assert!(victim, "precondition: a cold, old claim is a victim");
		assert_eq!(
			clock_skew_skips(),
			before,
			"a healthy clock must not read as a degradation"
		);
	}
	#[test]
	fn an_in_memory_kern_counts_what_it_drops_with_nowhere_to_spill() {
		// Spill-before-drop is a guarantee of a PERSISTED kern. With no store bound
		// there is nowhere to spill to and dropping is the intended memory bound —
		// but an in-memory deployment must not look durable, so the loss is counted.
		// Drives the real run_gc: a closure written in the test would prove nothing.
		let g = graph_with_cold_claim("victim");
		assert!(g.store().is_none(), "precondition: no cold store bound");
		let g = Arc::new(RwLock::new(g));

		let before = unspilled_drops();
		run_gc(&g, "k", &HeatConfig::default());

		assert!(
			!g.read()
				.kerns
				.get("k")
				.expect("kern k")
				.entities
				.contains_key("victim"),
			"precondition: the cold claim was actually evicted"
		);
		assert_eq!(
			unspilled_drops(),
			before + 1,
			"an unrecoverable drop must be countable, or in-memory reads as durable"
		);
	}

	#[test]
	fn evidence_decay_damps_alpha_beta_toward_prior_by_half_life() {
		use crate::base::types::{EntityStatus, Kern};
		let now = SystemTime::now();
		let seven_d = 7 * 24 * 60 * 60;
		let mut k = Kern::new("k", "");
		let mut e = ent(EntityKind::Fact, 0.0, Some(now));
		e.conf_alpha = 11.0;
		e.conf_beta = 3.0;
		e.updated_at = Some(now - Duration::from_secs(seven_d));
		e.status = EntityStatus::Active;
		k.entities.insert("e".into(), e);

		decay_evidence(&mut k, now, seven_d);
		let t = k.entities.get("e").unwrap();
		// (α-1)=10 halved → 5, so α ≈ 6.0; (β-1)=2 halved → 1, so β ≈ 2.0.
		assert!((t.conf_alpha - 6.0).abs() < 1e-4, "alpha {}", t.conf_alpha);
		assert!((t.conf_beta - 2.0).abs() < 1e-4, "beta {}", t.conf_beta);
		assert!((t.score - t.conf_mean()).abs() < 1e-9, "score refreshed");
	}

	#[test]
	fn evidence_decay_half_life_zero_is_a_noop() {
		use crate::base::types::Kern;
		let now = SystemTime::now();
		let mut k = Kern::new("k", "");
		let mut e = ent(EntityKind::Fact, 0.0, Some(now));
		e.conf_alpha = 11.0;
		e.conf_beta = 3.0;
		e.updated_at = Some(now - Duration::from_secs(7 * 24 * 60 * 60));
		k.entities.insert("e".into(), e);
		let before = k.entities.get("e").unwrap().clone();
		decay_evidence(&mut k, now, 0);
		let t = k.entities.get("e").unwrap();
		assert_eq!(t.conf_alpha, before.conf_alpha);
		assert_eq!(t.conf_beta, before.conf_beta);
		assert_eq!(t.score, before.score);
	}

	#[test]
	fn evidence_decay_skips_superseded_entities() {
		use crate::base::types::{EntityStatus, Kern};
		let now = SystemTime::now();
		let seven_d = 7 * 24 * 60 * 60;
		let mut k = Kern::new("k", "");
		let mut e = ent(EntityKind::Fact, 0.0, Some(now));
		e.conf_alpha = 11.0;
		e.conf_beta = 3.0;
		e.updated_at = Some(now - Duration::from_secs(seven_d));
		e.status = EntityStatus::Superseded;
		k.entities.insert("e".into(), e);
		let before = k.entities.get("e").unwrap().clone();
		decay_evidence(&mut k, now, seven_d);
		let t = k.entities.get("e").unwrap();
		assert_eq!(t.conf_alpha, before.conf_alpha, "superseded not decayed");
		assert_eq!(t.conf_beta, before.conf_beta);
	}
}
