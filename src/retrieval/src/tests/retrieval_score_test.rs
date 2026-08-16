//! Tests extracted from retrieval_score.rs
#![allow(unused)]
use super::*;

mod query_filter_tests {
	use super::*;
	use base::base_types::{Entity, Source};
	use std::collections::BTreeMap;

	fn ent(id: &str, kind: EntityKind, src: Source) -> ScoredEntity {
		ScoredEntity {
			entity: Entity {
				id: id.into(),
				kind,
				source: src,
				score: 0.5,
				..Default::default()
			},
			score: 1.0,
		}
	}

	fn file_src(path: &str) -> Source {
		Source::File {
			path: path.into(),
			section: String::new(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		}
	}

	fn ticket_src(id: &str) -> Source {
		Source::Ticket {
			system: "github".into(),
			object_id: id.into(),
			section: String::new(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		}
	}

	#[test]
	fn query_filter_by_kind_retains_only_matching() {
		let mut results = vec![
			ent("a", EntityKind::Fact, file_src("/a")),
			ent("b", EntityKind::Claim, file_src("/b")),
			ent("c", EntityKind::Question, ticket_src("123")),
		];
		let opts = QueryOptions {
			kind: Some(EntityKind::Fact),
			..QueryOptions::default()
		};
		apply_query_options(&mut results, &opts);
		assert_eq!(results.len(), 1);
		assert_eq!(results[0].entity.id, "a");
	}

	#[test]
	fn query_filter_by_scheme_retains_only_matching() {
		let mut results = vec![
			ent("a", EntityKind::Fact, file_src("/a")),
			ent("b", EntityKind::Claim, ticket_src("42")),
			ent("c", EntityKind::Document, file_src("/c")),
		];
		let opts = QueryOptions {
			scheme: Some("file".into()),
			..QueryOptions::default()
		};
		apply_query_options(&mut results, &opts);
		assert_eq!(results.len(), 2);
		assert!(results.iter().all(|r| r.entity.source.scheme() == "file"));
	}

	#[test]
	fn matches_filter_is_the_per_entity_predicate() {
		let fact_file = ent("a", EntityKind::Fact, file_src("/a")).entity;
		assert!(matches_filter(&fact_file, &QueryOptions::default()));
		assert!(matches_filter(
			&fact_file,
			&QueryOptions {
				kind: Some(EntityKind::Fact),
				..Default::default()
			}
		));
		assert!(!matches_filter(
			&fact_file,
			&QueryOptions {
				kind: Some(EntityKind::Claim),
				..Default::default()
			}
		));
		assert!(matches_filter(
			&fact_file,
			&QueryOptions {
				scheme: Some("file".into()),
				..Default::default()
			}
		));
		assert!(!matches_filter(
			&fact_file,
			&QueryOptions {
				scheme: Some("ticket".into()),
				..Default::default()
			}
		));
		assert!(matches_filter(
			&fact_file,
			&QueryOptions {
				min_conf: 0.4,
				..Default::default()
			}
		));
		assert!(!matches_filter(
			&fact_file,
			&QueryOptions {
				min_conf: 0.6,
				..Default::default()
			}
		));
		assert!(matches_filter(
			&fact_file,
			&QueryOptions {
				kind: Some(EntityKind::Fact),
				scheme: Some("file".into()),
				min_conf: 0.5,
				..Default::default()
			}
		));
	}

	#[test]
	fn claim_kind_filter_matches_the_session_title_label_and_drops_the_rest() {
		let labelled = ent(
			"a",
			EntityKind::Claim,
			Source::Session {
				session_id: "session:x".into(),
				section: String::new(),
				title: "session://code-fact".into(),
			},
		)
		.entity;
		let unlabelled = ent("b", EntityKind::Fact, file_src("/b")).entity;
		// The closure is pre-resolved by the tool layer; the predicate is pure
		// set membership over the label parsed out of `session://<kind>`.
		let want = QueryOptions {
			claim_kinds: Some(vec!["fact".into(), "code-fact".into()]),
			..Default::default()
		};
		assert!(matches_filter(&labelled, &want), "label in closure passes");
		assert!(
			!matches_filter(&unlabelled, &want),
			"an entity with no claim-kind label never matches a claim_kind filter"
		);
		let other = QueryOptions {
			claim_kinds: Some(vec!["preference".into()]),
			..Default::default()
		};
		assert!(
			!matches_filter(&labelled, &other),
			"label outside the closure drops"
		);
	}

	// Both halves matter. A pending entity that is never in the set proves nothing:
	// the same assertions pass against a predicate that was never written.
	#[test]
	fn exclude_pending_drops_only_the_uncurated_and_only_when_asked() {
		let active = ent("a", EntityKind::Claim, file_src("/a")).entity;
		let mut pending = ent("p", EntityKind::Claim, file_src("/p")).entity;
		pending.review = ReviewState::Pending;

		let on = QueryOptions {
			exclude_pending: true,
			..Default::default()
		};
		assert!(
			matches_filter(&active, &on),
			"a curated entity survives the filter"
		);
		assert!(
			!matches_filter(&pending, &on),
			"a pending entity is withheld once the caller asks to exclude it"
		);

		let off = QueryOptions::default();
		assert!(
			matches_filter(&pending, &off),
			"the same pending entity is returned when nobody asked — the filter is opt-in"
		);

		assert!(!off.is_active());
		assert!(
			on.is_active(),
			"an exclude_pending-only query must take the pre-filtered ANN path, not the unfiltered seed path"
		);
	}

	#[test]
	fn as_of_filters_across_open_and_closed_windows() {
		use std::time::{Duration, UNIX_EPOCH};
		let t = |s| UNIX_EPOCH + Duration::from_secs(s);

		let mut e = ent("a", EntityKind::Fact, file_src("/a")).entity;
		e.created_at = Some(t(100));

		assert!(!matches_filter(
			&e,
			&QueryOptions {
				as_of: Some(t(50)),
				..Default::default()
			}
		));
		assert!(matches_filter(
			&e,
			&QueryOptions {
				as_of: Some(t(100)),
				..Default::default()
			}
		));
		assert!(matches_filter(
			&e,
			&QueryOptions {
				as_of: Some(t(10_000)),
				..Default::default()
			}
		));

		e.valid_to = Some(t(200));
		assert!(matches_filter(
			&e,
			&QueryOptions {
				as_of: Some(t(150)),
				..Default::default()
			}
		));
		assert!(
			!matches_filter(
				&e,
				&QueryOptions {
					as_of: Some(t(200)),
					..Default::default()
				}
			),
			"valid_to is exclusive"
		);
		assert!(!matches_filter(
			&e,
			&QueryOptions {
				as_of: Some(t(500)),
				..Default::default()
			}
		));
		e.valid_from = Some(t(120));
		assert!(!matches_filter(
			&e,
			&QueryOptions {
				as_of: Some(t(110)),
				..Default::default()
			}
		));
	}

	#[test]
	fn filter_delivery_keeps_mmr_pool_when_mmr_enabled() {
		let cfg = RetrievalConfig::default();
		let mut results: Vec<ScoredEntity> = (0..60)
			.map(|i| ent(&format!("e{i}"), EntityKind::Fact, file_src("/x")))
			.collect();
		filter_delivery(&cfg, &mut results);
		assert_eq!(results.len(), cfg.mmr_pool_size);
	}

	#[test]
	fn filter_delivery_cuts_to_cap_when_mmr_disabled() {
		let cfg = RetrievalConfig {
			mmr_enabled: false,
			..Default::default()
		};
		let mut results: Vec<ScoredEntity> = (0..60)
			.map(|i| ent(&format!("e{i}"), EntityKind::Fact, file_src("/x")))
			.collect();
		filter_delivery(&cfg, &mut results);
		assert_eq!(results.len(), cfg.max_deliver_results);
	}

	#[test]
	fn commit_access_ids_stamps_the_live_entity_without_bumping_the_epoch() {
		use base::base_types::Kern;
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert(
			"a".into(),
			ent("a", EntityKind::Claim, file_src("/a")).entity,
		);
		g.kerns.insert("k".into(), k);
		g.index_entity("a", "k");
		let epoch_before = g.mutation_epoch();

		commit_access_ids(&mut g, &["a".to_string()], &HeatConfig::default());

		let live = g.kerns.get("k").unwrap().entities.get("a").unwrap();
		assert!(
			live.accessed_at.is_some(),
			"the LIVE entity gets a persisted accessed_at, not just the result copy"
		);
		assert_eq!(live.access_count.value(), 1, "live access counter bumped");
		assert!(live.heat > 0.0, "query heat deposited on the live entity");
		assert_eq!(
			g.mutation_epoch(),
			epoch_before,
			"access stamps must not invalidate the query cache"
		);
	}

	#[test]
	fn commit_access_ids_skips_ids_unknown_to_the_graph() {
		let mut g = GraphGnn::new();
		commit_access_ids(&mut g, &["ghost".to_string()], &HeatConfig::default());
	}

	#[test]
	fn qbst_zero_access_and_no_recency_is_zero() {
		let cfg = RetrievalConfig::default();
		assert_eq!(qbst(&cfg, 0, None), 0.0);
	}

	#[test]
	fn qbst_access_component_follows_log_count_times_weight() {
		let cfg = RetrievalConfig {
			qbst_access_weight: 1.5,
			qbst_recency_weight: 0.0,
			qbst_cap: 1e9,
			..Default::default()
		};
		let got = qbst(&cfg, 9, None);
		let expected = (9.0_f64 + 1.0).ln() * 1.5;
		assert!((got - expected).abs() < 1e-9, "got {got}, want {expected}");
	}

	#[test]
	fn qbst_recency_is_near_full_weight_at_zero_age() {
		let cfg = RetrievalConfig {
			qbst_access_weight: 0.0,
			qbst_recency_weight: 3.0,
			qbst_cap: 1e9,
			..Default::default()
		};
		let got = qbst(&cfg, 0, Some(SystemTime::now()));
		assert!(
			(got - 3.0).abs() < 0.05,
			"near-zero age -> ~full weight, got {got}"
		);
	}

	#[test]
	fn qbst_clamps_to_cap() {
		let cfg = RetrievalConfig {
			qbst_access_weight: 100.0,
			qbst_recency_weight: 100.0,
			qbst_cap: 2.0,
			..Default::default()
		};
		assert_eq!(
			qbst(&cfg, 1000, Some(SystemTime::now())),
			2.0,
			"clamped to qbst_cap"
		);
	}

	#[test]
	fn apply_boosts_scales_by_confidence_and_adds_fact_bonus_only_for_facts() {
		let cfg = RetrievalConfig {
			qbst_access_weight: 0.0,
			qbst_recency_weight: 0.0,
			fact_score_boost: 0.5,
			..Default::default()
		};
		let mut fact = ent("f", EntityKind::Fact, file_src("/f"));
		fact.score = 2.0;
		fact.entity.score = 0.5;
		let mut claim = ent("c", EntityKind::Claim, file_src("/c"));
		claim.score = 2.0;
		claim.entity.score = 0.5;
		let mut results = vec![fact, claim];
		apply_boosts(&cfg, &mut results);
		assert!(
			(results[0].score - 1.5).abs() < 1e-9,
			"fact got {}",
			results[0].score
		);
		assert!(
			(results[1].score - 1.0).abs() < 1e-9,
			"claim got {}",
			results[1].score
		);
	}

	// A single-observation claim must not outrank a well-evidenced one at equal
	// mean: the lower confidence bound subtracts K standard deviations, so the
	// tighter posterior wins. Negative control: with K=0 the two tie.
	#[test]
	fn lower_confidence_bound_ranks_well_evidenced_above_single_observation() {
		let cfg = RetrievalConfig {
			qbst_access_weight: 0.0,
			qbst_recency_weight: 0.0,
			fact_score_boost: 0.0,
			..Default::default()
		};
		// Both Beta priors share mean 2/3; the (20,10) one has ~3x tighter std.
		let mut single = ent("single", EntityKind::Claim, file_src("/s"));
		single.score = 1.0;
		single.entity.conf_alpha = 2.0;
		single.entity.conf_beta = 1.0;
		single.entity.refresh_score();
		let mut many = ent("many", EntityKind::Claim, file_src("/m"));
		many.score = 1.0;
		many.entity.conf_alpha = 20.0;
		many.entity.conf_beta = 10.0;
		many.entity.refresh_score();
		assert!(
			(single.entity.conf_mean() - many.entity.conf_mean()).abs() < 1e-9,
			"fixture must share a mean"
		);
		let mut results = vec![single, many];
		apply_boosts(&cfg, &mut results);
		assert!(
			results[1].score > results[0].score,
			"well-evidenced should outrank single-observation: many={} single={}",
			results[1].score,
			results[0].score
		);
	}

	// Bits, not a tolerance: the whole safety claim for source trust is that an
	// unconfigured kern ranks EXACTLY as it did before the knob existed.
	#[test]
	fn shipped_source_trust_default_leaves_boosted_scores_bit_identical() {
		let cfg = RetrievalConfig::default();
		assert!(
			cfg.source_trust.is_empty(),
			"the shipped default must weight no scheme, got {:?}",
			cfg.source_trust
		);
		let mut results = vec![
			ent("a", EntityKind::Fact, file_src("/a")),
			ent("b", EntityKind::Claim, ticket_src("42")),
			ent("c", EntityKind::Document, Source::default()),
		];
		for (i, r) in results.iter_mut().enumerate() {
			r.score = 0.25 * (i as f64 + 1.0);
			r.entity.score = 0.1 * (i as f64 + 3.0);
		}
		let expected: Vec<u64> = results
			.iter()
			.map(|r| {
				let fact_bonus = if r.entity.kind == EntityKind::Fact {
					cfg.fact_score_boost
				} else {
					0.0
				};
				// apply_boosts ranks on the lower confidence bound, not e.score.
				let confidence =
					(r.entity.conf_mean() - CONFIDENCE_BOUND_K * r.entity.conf_variance().sqrt()).max(0.0);
				(r.score * confidence + fact_bonus).to_bits()
			})
			.collect();

		apply_boosts(&cfg, &mut results);

		let got: Vec<u64> = results.iter().map(|r| r.score.to_bits()).collect();
		assert_eq!(got, expected, "an unconfigured source_trust moved a score");
	}

	// The other half: a knob that only ever proves it does nothing is satisfied by
	// code that does nothing.
	#[test]
	fn a_configured_source_trust_reorders_two_otherwise_equal_entities() {
		let watched = ent("watched", EntityKind::Claim, file_src("/notes.md"));
		let typed = ent("typed", EntityKind::Claim, Source::default());

		let mut tied = vec![watched.clone(), typed.clone()];
		apply_boosts(&RetrievalConfig::default(), &mut tied);
		assert_eq!(
			tied[0].score, tied[1].score,
			"the two differ only by source scheme, so unconfigured they must tie"
		);

		let cfg = RetrievalConfig {
			source_trust: BTreeMap::from([("file".to_string(), 0.5)]),
			..Default::default()
		};
		let mut results = vec![watched, typed];
		apply_boosts(&cfg, &mut results);
		assert!(
			results[1].score > results[0].score,
			"the file-scheme entity must fall below the inline one: {} vs {}",
			results[0].score,
			results[1].score
		);
		assert_eq!(
			results[0].score.to_bits(),
			(tied[0].score * 0.5).to_bits(),
			"the weighted score is the composite scaled by the configured trust"
		);
	}
	// The cap the CLI hands a serving daemon has to be the cap the local read
	// applies, so it is read from here rather than restated at the call site.
	#[test]
	fn delivery_cap_is_the_pool_mmr_keeps_and_the_cut_it_applies() {
		let cfg = RetrievalConfig {
			mmr_enabled: true,
			mmr_pool_size: 50,
			max_deliver_results: 25,
			min_deliver_score: 0.0,
			..Default::default()
		};
		assert_eq!(delivery_cap(&cfg), 50, "MMR keeps the larger pool");

		let mut results: Vec<_> = (0..60)
			.map(|i| ent(&format!("e{i}"), EntityKind::Claim, file_src("/a")))
			.collect();
		filter_delivery(&cfg, &mut results);
		assert_eq!(
			results.len(),
			delivery_cap(&cfg),
			"the cut is that same cap"
		);

		let off = RetrievalConfig {
			mmr_enabled: false,
			..cfg
		};
		assert_eq!(
			delivery_cap(&off),
			25,
			"without MMR the delivery cap stands alone"
		);
	}

	#[test]
	fn a_delivery_that_bypasses_the_floor_is_counted() {
		let cfg = RetrievalConfig {
			min_deliver_score: 5.0,
			..Default::default()
		};
		let mut results = vec![ent("a", EntityKind::Claim, file_src("/a"))];
		results[0].score = 0.1;

		let before = below_floor_deliveries();
		filter_delivery(&cfg, &mut results);

		assert_eq!(
			results.len(),
			1,
			"fail-open: the below-floor set is still delivered"
		);
		assert_eq!(
			below_floor_deliveries(),
			before + 1,
			"but the bypass is counted, so a degraded answer is distinguishable"
		);
	}

	#[test]
	fn a_delivery_that_clears_the_floor_is_not_counted() {
		let cfg = RetrievalConfig {
			min_deliver_score: 0.05,
			..Default::default()
		};
		let mut results = vec![ent("a", EntityKind::Claim, file_src("/a"))];
		results[0].score = 0.1;

		let before = below_floor_deliveries();
		filter_delivery(&cfg, &mut results);
		assert_eq!(results.len(), 1);
		assert_eq!(
			below_floor_deliveries(),
			before,
			"a normal delivery must not read as a degradation"
		);
	}
	#[test]
	fn an_expired_claim_is_dropped_on_the_default_path() {
		let now = SystemTime::now();
		let mut live = ent("live", EntityKind::Claim, file_src("/a"));
		let mut expired = ent("expired", EntityKind::Claim, file_src("/b"));
		expired.entity.valid_until = Some(now - Duration::from_secs(60));
		live.entity.valid_until = Some(now + Duration::from_secs(60));
		let mut results = vec![live, expired];

		drop_expired(&mut results, None, now);

		let ids: Vec<&str> = results.iter().map(|r| r.entity.id.as_str()).collect();
		assert_eq!(
			ids,
			vec!["live"],
			"an expired claim must not rank when no caller asked about time"
		);
	}

	#[test]
	fn an_entity_with_no_ttl_is_never_dropped() {
		let now = SystemTime::now();
		let mut results = vec![ent("forever", EntityKind::Claim, file_src("/a"))];
		assert!(results[0].entity.valid_until.is_none(), "precondition");
		drop_expired(&mut results, None, now);
		assert_eq!(results.len(), 1);
	}

	#[test]
	fn a_point_in_time_query_still_sees_a_since_expired_claim() {
		let now = SystemTime::now();
		let mut expired = ent("expired", EntityKind::Claim, file_src("/b"));
		expired.entity.valid_until = Some(now - Duration::from_secs(60));
		let mut results = vec![expired];

		let opts = QueryOptions {
			as_of: Some(now - Duration::from_secs(3600)),
			..Default::default()
		};
		drop_expired(&mut results, Some(&opts), now);

		assert_eq!(
			results.len(),
			1,
			"as_of judges validity at ITS instant — expiring it against now would \
			 make history unqueryable, which is the opposite of the guarantee"
		);
	}

	#[test]
	fn an_explicit_valid_at_is_left_to_matches_filter() {
		let now = SystemTime::now();
		let mut expired = ent("expired", EntityKind::Claim, file_src("/b"));
		expired.entity.valid_until = Some(now - Duration::from_secs(60));
		let mut results = vec![expired];

		let opts = QueryOptions {
			valid_at: Some(now - Duration::from_secs(3600)),
			..Default::default()
		};
		drop_expired(&mut results, Some(&opts), now);
		assert_eq!(results.len(), 1, "the caller named the instant; honour it");
	}
	#[test]
	fn replaying_a_query_cannot_pump_one_thoughts_access_count() {
		let mut e = ent("hot", EntityKind::Claim, file_src("/a")).entity;
		let now = SystemTime::now();
		let hl = HeatConfig::default();

		assert!(stamp_access(&mut e, now, &hl), "the first access counts");
		let after_first = e.access_count.value_i32();
		let heat_after_first = e.heat;

		for _ in 0..50 {
			assert!(
				!stamp_access(&mut e, now, &hl),
				"a replay inside the window is suppressed"
			);
		}

		assert_eq!(
			e.access_count.value_i32(),
			after_first,
			"50 replays must not move the count"
		);
		assert_eq!(e.heat, heat_after_first, "nor the heat");
	}

	#[test]
	fn genuine_reuse_after_the_window_still_counts() {
		let mut e = ent("used", EntityKind::Claim, file_src("/a")).entity;
		let hl = HeatConfig::default();
		let now = SystemTime::now();

		assert!(stamp_access(&mut e, now, &hl));
		let first = e.access_count.value_i32();

		let later = now + ACCESS_COOLDOWN + Duration::from_secs(1);
		assert!(
			stamp_access(&mut e, later, &hl),
			"use outside the window is real use, not a replay"
		);
		assert_eq!(e.access_count.value_i32(), first + 1);
	}

	#[test]
	fn a_never_accessed_thought_is_not_throttled() {
		let mut e = ent("fresh", EntityKind::Claim, file_src("/a")).entity;
		assert!(e.accessed_at.is_none(), "precondition");
		assert!(stamp_access(
			&mut e,
			SystemTime::now(),
			&HeatConfig::default()
		));
	}
}
mod lexical_boost_tests {
	use super::*;

	fn scored(id: &str, score: f64) -> ScoredEntity {
		ScoredEntity {
			entity: Entity {
				id: id.into(),
				..Default::default()
			},
			score,
		}
	}

	#[test]
	fn zero_weight_is_a_noop() {
		let lex = LexicalIndex::new_in_ram(1.2, 0.75);
		lex.insert("a", "the quick brown fox");
		lex.insert("b", "lazy dog sleeps");
		let mut results = vec![scored("a", 0.9), scored("b", 0.8)];
		let cfg = RetrievalConfig {
			lexical_top_boost: 0.0,
			..Default::default()
		};
		apply_lexical_boost(&lex, &cfg, "quick fox", &mut results);
		assert_eq!(results[0].score, 0.9);
		assert_eq!(results[1].score, 0.8);
	}

	#[test]
	fn no_query_terms_in_corpus_is_a_noop() {
		let lex = LexicalIndex::new_in_ram(1.2, 0.75);
		lex.insert("a", "the quick brown fox");
		let mut results = vec![scored("a", 0.9)];
		let cfg = RetrievalConfig {
			lexical_top_boost: 1.0,
			..Default::default()
		};
		apply_lexical_boost(&lex, &cfg, "zzz nonexistent", &mut results);
		assert_eq!(results[0].score, 0.9, "no BM25 hit => no bonus");
	}

	#[test]
	fn exact_match_gets_the_full_bonus_others_get_less() {
		let lex = LexicalIndex::new_in_ram(1.2, 0.75);
		lex.insert("match", "alice bought a red car in paris");
		lex.insert("partial", "alice visited paris once");
		lex.insert("none", "bob likes hiking");
		// Start them equal; the BM25 bonus alone must order them.
		let mut results = vec![
			scored("none", 0.5),
			scored("partial", 0.5),
			scored("match", 0.5),
		];
		let cfg = RetrievalConfig {
			lexical_top_boost: 1.0,
			..Default::default()
		};
		apply_lexical_boost(&lex, &cfg, "alice red car paris", &mut results);
		results.sort_by(|a, b| {
			b.score
				.partial_cmp(&a.score)
				.unwrap_or(std::cmp::Ordering::Equal)
		});
		assert_eq!(
			results[0].entity.id, "match",
			"the verbatim-overlap doc wins the top"
		);
		assert_eq!(
			results.last().unwrap().entity.id,
			"none",
			"the no-overlap doc stays last"
		);
		assert!(results[0].score > results.last().unwrap().score);
	}
}
