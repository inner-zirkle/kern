//! Tests extracted from commands_admin.rs
#![allow(unused)]
use super::*;

mod degradation_lines_tests {
	use super::*;
	use ::health::HealthStats;
	use transport::kern_rpc::HealthRes;

	// What a CLI can actually see of the eight: it opened its own store and ran
	// no query, no tick and no ingest, so every one of them is zero.
	fn blind_cli() -> HealthStats {
		HealthStats::default()
	}

	#[test]
	fn a_serving_daemons_counts_win_over_this_processs_zeros() {
		let daemon = HealthRes {
			ok: true,
			cold_evicted: 3,
			ingest_dropped_chunks: 9,
			..Default::default()
		};
		let lines = degradation_lines(&blind_cli(), Some(&daemon));
		assert_eq!(lines[0], "evicted:     3 cold rows dropped");
		assert!(
			lines[1].contains("9 chunks lost to embedding"),
			"the daemon's drops reach the operator reading them: {lines:?}"
		);
	}

	#[test]
	fn a_local_count_is_not_printed_over_a_serving_daemons() {
		// The inverted case, which a merge of the two sources would pass: the
		// daemon is healthy and this process is not, and the daemon is what runs.
		let local = HealthStats {
			cold_evicted: 4,
			unspilled_drops: 6,
			..Default::default()
		};
		let daemon = HealthRes {
			ok: true,
			..Default::default()
		};
		let lines = degradation_lines(&local, Some(&daemon));
		assert_eq!(lines[0], "evicted:     0 cold rows dropped");
		assert_eq!(lines.len(), 1, "nothing degraded over there: {lines:?}");
	}

	#[test]
	fn with_nothing_serving_the_local_counts_still_stand() {
		let local = HealthStats {
			cold_evicted: 2,
			unspilled_drops: 5,
			..Default::default()
		};
		let lines = degradation_lines(&local, None);
		assert_eq!(lines[0], "evicted:     2 cold rows dropped");
		assert!(
			lines[1].contains("5 dropped with nowhere to spill"),
			"the offline path is unchanged: {lines:?}"
		);
	}

	#[test]
	fn a_healthy_kern_prints_no_degraded_line_from_either_source() {
		assert_eq!(degradation_lines(&blind_cli(), None).len(), 1);
		let healthy = HealthRes {
			ok: true,
			..Default::default()
		};
		assert_eq!(degradation_lines(&blind_cli(), Some(&healthy)).len(), 1);
	}

	#[test]
	fn kern_health_warns_when_resident_kerns_approach_cap() {
		// 116/128 >= 0.9*128 (115.2) -> warn line present.
		let near = HealthRes {
			ok: true,
			kerns: 116,
			max_kerns: 128,
			..Default::default()
		};
		let lines = kern_cap_health_lines(Some(&near));
		assert_eq!(lines, vec!["kerns near cap: 116/128"]);

		// 10/128 -> no warn.
		let fine = HealthRes {
			ok: true,
			kerns: 10,
			max_kerns: 128,
			..Default::default()
		};
		assert!(
			kern_cap_health_lines(Some(&fine)).is_empty(),
			"under the approach fraction -> no warn"
		);

		// KERN_CAP_DISABLED (u64::MAX) -> cap off, no warn.
		let uncapped = HealthRes {
			ok: true,
			kerns: 1000,
			max_kerns: u64::MAX,
			..Default::default()
		};
		assert!(
			kern_cap_health_lines(Some(&uncapped)).is_empty(),
			"uncapped -> no warn"
		);

		// 0 (old daemon / unset) -> no warn.
		let old = HealthRes {
			ok: true,
			kerns: 1000,
			max_kerns: 0,
			..Default::default()
		};
		assert!(
			kern_cap_health_lines(Some(&old)).is_empty(),
			"absent cap field -> no warn"
		);

		// No daemon -> no warn.
		assert!(kern_cap_health_lines(None).is_empty());
	}

	#[test]
	fn kern_health_prints_heat_half_life() {
		// 30d (relaxed preset) -> half-life 2592000s, line present.
		let relaxed = HealthRes {
			ok: true,
			heat_half_life_secs: 2592000,
			..Default::default()
		};
		let lines = heat_health_lines(Some(&relaxed));
		assert_eq!(
			lines,
			vec![
				"preset:      ",
				"heat:        half-life 2592000s",
				"recency:     half-life 0s",
			]
		);

		// 0 (old daemon / unset) -> prints 0s unconditionally, matching the
		// convergence: line that prints `gini 0.00` when a daemon answers.
		let old = HealthRes {
			ok: true,
			heat_half_life_secs: 0,
			..Default::default()
		};
		assert_eq!(
			heat_health_lines(Some(&old)),
			vec![
				"preset:      ",
				"heat:        half-life 0s",
				"recency:     half-life 0s",
			],
			"a daemon that answers carries the line even at 0"
		);

		// No daemon -> no line: the CLI's own config is irrelevant.
		assert!(
			heat_health_lines(None).is_empty(),
			"no daemon -> no heat line"
		);

		// 24h QBST recency half-life -> the recency line carries it (ROADMAP
		// item 55 measurement half). The heat line stays at its own value.
		let recency = HealthRes {
			ok: true,
			heat_half_life_secs: 2592000,
			qbst_recency_half_life_secs: 86400,
			..Default::default()
		};
		assert_eq!(
			heat_health_lines(Some(&recency)),
			vec![
				"preset:      ",
				"heat:        half-life 2592000s",
				"recency:     half-life 86400s",
			],
			"recency half-life surfaced daemon-sourced"
		);
	}

	#[test]
	fn kern_health_prints_source_trust() {
		// A configured source-trust map prints one line naming each scheme.
		let cfg = HealthRes {
			ok: true,
			source_trust: std::collections::BTreeMap::from([
				("file".to_string(), 0.8),
				("ticket".to_string(), 0.9),
			]),
			..Default::default()
		};
		assert_eq!(
			source_trust_health_lines(Some(&cfg)),
			vec!["source_trust: file=0.8, ticket=0.9"],
		);

		// An unconfigured kern (empty map) prints a (none) line — the
		// bit-identical default, not absent.
		let uncfg = HealthRes {
			ok: true,
			..Default::default()
		};
		assert_eq!(
			source_trust_health_lines(Some(&uncfg)),
			vec!["source_trust: (none)"],
			"empty map surfaces a (none) line, matching the zeroed-value rule"
		);

		// No daemon -> no line (item 100 rule: the CLI's own config is
		// irrelevant, the daemon's running map is what the operator asked
		// about).
		assert!(
			source_trust_health_lines(None).is_empty(),
			"no daemon -> no source_trust line"
		);
	}

	#[test]
	fn kern_health_prints_dedup_config() {
		// A configured per-kind override prints one line with the global + the
		// override (ROADMAP item 48 measurement half).
		let cfg = HealthRes {
			ok: true,
			ingest_dedup_threshold: 0.95,
			ingest_dedup_threshold_by_kind: [Some(0.99), None, None, None, None],
			..Default::default()
		};
		assert_eq!(
			dedup_health_lines(Some(&cfg)),
			vec!["dedup: 0.95, kind fact=0.99"],
		);

		// All-None -> global only, no kind suffix.
		let uncfg = HealthRes {
			ok: true,
			ingest_dedup_threshold: 0.95,
			..Default::default()
		};
		assert_eq!(
			dedup_health_lines(Some(&uncfg)),
			vec!["dedup: 0.95"],
			"all-None per-kind surfaces the global only"
		);

		// No daemon -> no line (item 100 rule).
		assert!(
			dedup_health_lines(None).is_empty(),
			"no daemon -> no dedup line"
		);
	}

	#[test]
	fn kern_health_prints_preset() {
		// A named preset -> the line carries it (ROADMAP item 87 measurement half).
		let tight = HealthRes {
			ok: true,
			preset: "tight".into(),
			..Default::default()
		};
		let lines = heat_health_lines(Some(&tight));
		assert!(
			lines.iter().any(|l| l == "preset:      tight"),
			"preset name surfaced: {lines:?}"
		);

		// Relaxed (the default) -> relaxed.
		let relaxed = HealthRes {
			ok: true,
			preset: "relaxed".into(),
			..Default::default()
		};
		assert!(
			heat_health_lines(Some(&relaxed))
				.iter()
				.any(|l| l == "preset:      relaxed"),
			"relaxed preset named"
		);

		// Empty (old daemon) -> empty name, line still present.
		let old = HealthRes {
			ok: true,
			preset: String::new(),
			..Default::default()
		};
		assert!(
			heat_health_lines(Some(&old))
				.iter()
				.any(|l| l == "preset:      "),
			"old daemon -> empty preset name"
		);

		// No daemon -> no line.
		assert!(
			!heat_health_lines(None)
				.iter()
				.any(|l| l.starts_with("preset:")),
			"no daemon -> no preset line"
		);
	}

	#[test]
	fn kern_health_prints_retrieval_config() {
		// Non-default retrieval block -> the four lines carry it daemon-sourced.
		let cfg = HealthRes {
			ok: true,
			retrieval: transport::kern_rpc::RetrievalHealth {
				rrf_k: 60.0,
				rrf_global_weight: 0.5,
				weights_content: transport::kern_rpc::ModeWeightsHealth {
					content: 0.7,
					reason: 0.2,
					edge: 0.1,
				},
				weights_reason: transport::kern_rpc::ModeWeightsHealth {
					content: 0.1,
					reason: 0.8,
					edge: 0.1,
				},
				weights_hybrid: transport::kern_rpc::ModeWeightsHealth {
					content: 0.5,
					reason: 0.3,
					edge: 0.2,
				},
				seed_k: 30,
				mmr_enabled: false,
				lexical_enabled: true,
				pagerank_enabled: true,
			},
			..Default::default()
		};
		let lines = retrieval_health_lines(Some(&cfg));
		assert_eq!(lines.len(), 5, "header + three mode-weight + one knob line");
		assert!(
			lines[0].contains("retrieval:") && lines[0].contains("60") && lines[0].contains("0.5"),
			"header carries rrf_k + global: {lines:?}"
		);
		assert!(
			lines[1].contains("content") && lines[1].contains("0.7"),
			"content weights line: {lines:?}"
		);
		assert!(
			lines[2].contains("reason") && lines[2].contains("0.8"),
			"reason weights line: {lines:?}"
		);
		assert!(
			lines[3].contains("hybrid") && lines[3].contains("0.2"),
			"hybrid weights line: {lines:?}"
		);
		assert!(
			lines[4].contains("seed_k 30")
				&& lines[4].contains("mmr false")
				&& lines[4].contains("lexical true")
				&& lines[4].contains("pagerank true"),
			"knob line carries the four: {lines:?}"
		);

		// Zeroed (old daemon / unset) -> five lines of zeroes, matching the
		// heat/recency lines that print `0s` when a daemon answers.
		let old = HealthRes {
			ok: true,
			..Default::default()
		};
		assert_eq!(
			retrieval_health_lines(Some(&old)).len(),
			5,
			"old daemon still prints the block at zero"
		);

		// No daemon -> no block: the CLI's own config is irrelevant.
		assert!(
			retrieval_health_lines(None).is_empty(),
			"no daemon -> no retrieval block"
		);
	}
}
mod cmd_tests {
	use super::*;
	use config::Config;

	fn temp_cfg() -> (tempfile::TempDir, Config) {
		let dir = tempfile::tempdir().expect("tempdir");
		let cfg = Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};
		(dir, cfg)
	}

	#[cfg(unix)]
	#[tokio::test(flavor = "multi_thread")]
	async fn claim_kind_add_then_remove_persists_through_the_graph() {
		let (_dir, cfg) = temp_cfg();
		// An endpoint nothing ever bound: the NoDaemon fallback, pinned so the
		// test can never reach a daemon the developer happens to be running.
		let ep = crate::test_helpers::scratch_endpoint("claim-kind-local");
		// A custom key, not a default: default keys re-inject on every load, so Rm
		// would appear to fail on the next load.
		let key = "custom_test_kind";

		claim_kind_at(
			&cfg,
			&ep,
			ClaimKindAction::Add {
				name: key.into(),
				description: "a custom kind".into(),
				parent: None,
			},
		)
		.await;
		let g = load_graph(&cfg);
		assert_eq!(
			g.root.claim_kinds.get(key).map(String::as_str),
			Some("a custom kind"),
			"Add persists the claim kind onto the root",
		);

		claim_kind_at(&cfg, &ep, ClaimKindAction::Rm { name: key.into() }).await;
		let g = load_graph(&cfg);
		assert!(
			!g.root.claim_kinds.contains_key(key),
			"Rm removes the custom claim kind"
		);
	}

	// The half of item 9 this closes: beside a serving daemon the command must
	// hand the write over, because the local path is `with_graph` — load, mutate,
	// `save_graph_unguarded` — which writes the whole kern map back with no epoch
	// check and drops every commit the daemon made since that load.
	#[cfg(unix)]
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_claim_kind_add_lands_in_the_daemon_and_never_touches_the_store() {
		let (_dir, cfg) = temp_cfg();
		let ep = crate::test_helpers::scratch_endpoint("claim-kind-routed");
		let srv = crate::test_helpers::rpc_server();
		let graph = srv.graph.clone();
		crate::test_helpers::serving(srv, &ep).await;

		claim_kind_at(
			&cfg,
			&ep,
			ClaimKindAction::Add {
				name: "custom_test_kind".into(),
				description: "a custom kind".into(),
				parent: None,
			},
		)
		.await;

		assert_eq!(
			graph
				.read()
				.root
				.claim_kinds
				.get("custom_test_kind")
				.map(String::as_str),
			Some("a custom kind"),
			"the serving daemon's own graph took the write"
		);
		assert!(
			!load_graph(&cfg)
				.root
				.claim_kinds
				.contains_key("custom_test_kind"),
			"the CLI's store was never written behind the daemon's back"
		);
	}

	#[cfg(unix)]
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_graviton_remove_lands_in_the_daemon_and_never_touches_the_store() {
		let (_dir, cfg) = temp_cfg();
		// The local store carries the same graviton, so a command that fell through
		// to `with_graph` would visibly delete it here.
		with_graph(&cfg, |g| {
			graph::accept::add_graviton_with_mass(g, "docs", vec![1.0, 0.0], 1.0)
		});

		let ep = crate::test_helpers::scratch_endpoint("graviton-routed");
		let srv = crate::test_helpers::rpc_server();
		let graph = srv.graph.clone();
		graph::accept::add_graviton_with_mass(&mut graph.write(), "docs", vec![1.0, 0.0], 1.0);
		crate::test_helpers::serving(srv, &ep).await;

		graviton_at(
			&cfg,
			&ep,
			GravitonAction::Remove {
				name: "docs".into(),
			},
		)
		.await;

		assert!(
			graviton_rows(&graph.read()).is_empty(),
			"the serving daemon's own graph lost the graviton"
		);
		assert_eq!(
			graviton_rows(&load_graph(&cfg))
				.iter()
				.map(|r| r.name.clone())
				.collect::<Vec<_>>(),
			vec!["docs".to_string()],
			"the CLI's store is untouched — the daemon owns the write"
		);
	}

	#[tokio::test]
	async fn cmd_health_runs_on_a_fresh_graph_without_panicking() {
		let (_dir, cfg) = temp_cfg();
		cmd_health(&cfg).await;
	}

	#[test]
	fn tick_health_lines_report_both_degradation_counters() {
		let offline = tick_health_lines(None);
		assert_eq!(offline.len(), 1, "no daemon -> no invented numbers");
		assert!(offline[0].contains("no daemon"), "{offline:?}");

		let live = tick_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			task_panics: 2,
			last_task_panic: "GnnPropagate[k]: boom".into(),
			task_failures: 3,
			last_task_failure: "GnnPropagate[k]: train epoch 0 forward".into(),
			..Default::default()
		}))
		.join("\n");
		assert!(live.contains("2 panics | 3 failures"), "{live}");
		assert!(
			live.contains("last panic:   GnnPropagate[k]: boom"),
			"{live}"
		);
		assert!(
			live.contains("last failure: GnnPropagate[k]: train epoch 0 forward"),
			"{live}"
		);
	}

	// This counter alone, every other one zero — which is the only state it is
	// ever seen in, since the trainer refusing has nothing to do with a task
	// panicking. A line gated on some other counter reports nothing here.
	#[test]
	fn a_refused_gnn_training_shows_with_no_other_counter_moving() {
		let lines = tick_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			gnn_train_refused: 4,
			..Default::default()
		}));
		assert_eq!(lines.len(), 2, "counts only, no fault lines: {lines:?}");
		assert!(lines[1].contains("4 refused GNN trainings"), "{lines:?}");
	}

	#[test]
	fn the_ingest_line_prints_the_daemons_depth_and_nothing_without_one() {
		assert!(
			ingest_health_lines(None).is_empty(),
			"no daemon -> no invented numbers"
		);
		let lines = ingest_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			ingest_queue_depth: 5,
			..Default::default()
		}));
		assert_eq!(lines, vec!["ingest:      queue 5".to_string()]);
	}

	// The three outcomes item 30 says were one empty string. Each drives the same
	// counter, so the count alone cannot tell them apart — the named last failure
	// is what does, and the test would pass on a count-only surface without it.
	#[test]
	fn the_llm_line_names_which_failure_it_counted() {
		assert!(
			llm_health_lines(None).is_empty(),
			"no daemon -> no invented numbers"
		);
		assert!(
			llm_health_lines(Some(&transport::kern_rpc::HealthRes {
				ok: true,
				..Default::default()
			}))
			.is_empty(),
			"a healthy completion leg stays quiet"
		);

		for reason in [
			"transient: HTTP error: operation timed out",
			"transient: HTTP error: tcp connect error",
			"permanent: empty completion response",
		] {
			let lines = llm_health_lines(Some(&transport::kern_rpc::HealthRes {
				ok: true,
				llm_complete_failed: 7,
				last_llm_complete_failure: reason.into(),
				..Default::default()
			}));
			assert_eq!(lines.len(), 2, "{lines:?}");
			assert!(lines[0].contains("7 failed completions"), "{lines:?}");
			assert!(lines[1].contains(reason), "{lines:?}");
		}
	}

	#[test]
	fn a_clean_daemon_prints_no_last_fault_lines() {
		let lines = tick_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			..Default::default()
		}));
		assert_eq!(
			lines.len(),
			2,
			"healthy tick reports counts only: {lines:?}"
		);
	}
}
mod hub_merge_tests {
	use base::base_types::{mk_entity, EntityKind, Kern};

	fn store_with_entity(root: &std::path::Path, eid: &str) {
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let cfg = config::Config::default_in(root);
		let mut g = graph::graph::GraphGnn::new();
		g.data_dir = cfg.data_dir.clone();
		std::fs::create_dir_all(&g.data_dir).unwrap();
		let mut k = Kern::new("k-hub-merge", g.root.id.clone());
		k.root_id = g.root.id.clone();
		k.graviton_text = "merge test".into();
		k.entities.insert(
			eid.to_string(),
			mk_entity(eid, "merged fact", 1.0, EntityKind::Fact),
		);
		g.register(k);
		// save_all silently no-ops without a store attached.
		let store = store_core::Store::open(&g.data_dir).unwrap();
		g.set_store(std::sync::Arc::new(store));
		graph::persist::save_all(&g).unwrap();
	}

	fn dst_entities(root: &std::path::Path) -> usize {
		let cfg = config::Config::default_in(root);
		let g = crate::load_graph(&cfg);
		::health::graph_health_stats(&g).entities
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn merge_absorbs_src_entities_into_dst_and_leaves_src_alone() {
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("src");
		let dst = dir.path().join("dst");
		store_with_entity(&src, "e-src");
		store_with_entity(&dst, "e-dst");
		assert_eq!(dst_entities(&src), 1, "src store persisted before merge");

		super::cmd_hub_merge(&src.display().to_string(), &dst.display().to_string()).await;

		assert_eq!(
			dst_entities(&dst),
			2,
			"dst holds its own + the absorbed entity"
		);
		assert_eq!(dst_entities(&src), 1, "src is never written");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn merge_refuses_identical_roots_and_missing_src() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("only");
		store_with_entity(&root, "e-1");
		let r = root.display().to_string();

		// Same root: refused before any store is touched.
		super::cmd_hub_merge(&r, &r).await;
		assert_eq!(dst_entities(&root), 1, "self-merge is a refused no-op");

		// Missing src: refused.
		super::cmd_hub_merge("/nonexistent/kern-merge-src", &r).await;
		assert_eq!(dst_entities(&root), 1, "missing src leaves dst untouched");
	}
}
