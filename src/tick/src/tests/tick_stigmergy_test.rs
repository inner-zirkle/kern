//! Tests extracted from tick_stigmergy.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::Kern;
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
	fn mixed_population(dir: &tempfile::TempDir) -> (GraphGnn, Arc<store_core::Store>) {
		use base::base_types::EntityStatus;
		use store_core::Store;

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
			.filter(|e| is_cold_victim(e, now, HL))
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

	fn cold_ids(s: &store_core::Store) -> Vec<String> {
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
			|_| Err(store_core::StoreError::BadVersion(9)),
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
			HL
		));
	}

	#[test]
	fn heat_above_threshold_is_preserved_even_when_old() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		let mut hot = ent(EntityKind::Claim, 1e9, Some(old));
		hot.heat_updated_at = Some(now);
		assert!(!is_cold_victim(&hot, now, HL));
	}

	#[test]
	fn stale_heat_decays_away_and_stops_shielding_the_entity() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		let mut once_hot = ent(EntityKind::Claim, 1e9, Some(old));
		once_hot.heat_updated_at = Some(old);
		assert!(
			is_cold_victim(&once_hot, now, HL),
			"heat last deposited a week ago has decayed below the threshold; \
			 raw stored heat must not grant permanent GC immunity"
		);
	}

	#[test]
	fn durable_kinds_are_never_collected() {
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		assert!(
			!is_cold_victim(&ent(EntityKind::Fact, 0.0, Some(old)), now, HL),
			"Fact preserved"
		);
		assert!(
			!is_cold_victim(&ent(EntityKind::Document, 0.0, Some(old)), now, HL),
			"Document preserved"
		);
	}

	#[test]
	fn superseded_fact_loses_immunity_and_becomes_a_victim() {
		use base::base_types::EntityStatus;
		let now = SystemTime::now();
		let old = now - (COLD_GC_AGE + Duration::from_secs(1));
		assert!(
			!is_cold_victim(&ent(EntityKind::Fact, 0.0, Some(old)), now, HL),
			"active Fact is immune even when stale"
		);
		let mut superseded = ent(EntityKind::Fact, 0.0, Some(old));
		superseded.status = EntityStatus::Superseded;
		assert!(
			is_cold_victim(&superseded, now, HL),
			"a superseded (invalidated) Fact is no longer immune"
		);
		let mut fresh_superseded = ent(EntityKind::Fact, 0.0, Some(now));
		fresh_superseded.status = EntityStatus::Superseded;
		assert!(
			!is_cold_victim(&fresh_superseded, now, HL),
			"a recently-touched superseded fact is still spared"
		);
	}

	#[test]
	fn run_gc_spills_superseded_fact_to_cold_while_active_fact_stays_immune() {
		use base::base_types::EntityStatus;
		use parking_lot::RwLock;
		use std::sync::Arc;
		use store_core::Store;

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
			!is_cold_victim(&ent(EntityKind::Claim, 0.0, Some(now)), now, HL),
			"recently accessed"
		);
		assert!(
			!is_cold_victim(&ent(EntityKind::Claim, 0.0, None), now, HL),
			"no timestamps at all"
		);
		let future = now + Duration::from_secs(3600);
		assert!(
			!is_cold_victim(&ent(EntityKind::Claim, 0.0, Some(future)), now, HL),
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
			is_cold_victim(&stale, now, HL),
			"old-but-never-queried is a victim"
		);
		let mut fresh = ent(EntityKind::Claim, 0.0, None);
		fresh.created_at = Some(now);
		assert!(
			!is_cold_victim(&fresh, now, HL),
			"fresh ingest is preserved"
		);
		let mut touched = ent(EntityKind::Claim, 0.0, Some(now));
		touched.created_at = Some(old);
		assert!(
			!is_cold_victim(&touched, now, HL),
			"accessed_at takes precedence over created_at"
		);
	}

	#[test]
	fn run_gc_spills_stale_victim_to_cold_store_and_spares_facts() {
		use parking_lot::RwLock;
		use std::sync::Arc;
		use store_core::Store;

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
		let victim = is_cold_victim(&e, SystemTime::now(), HeatConfig::default().half_life_secs);

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
		let victim = is_cold_victim(&e, SystemTime::now(), HeatConfig::default().half_life_secs);

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
		use base::base_types::{EntityStatus, Kern};
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
		use base::base_types::Kern;
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
		use base::base_types::{EntityStatus, Kern};
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
