//! Tests extracted from commands_graph_ops.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	#[allow(unused_imports)]
	use base::base_constants::{
		DEGRADE_DECAY_BASE, DEGRADE_DECAY_POW, DEGRADE_FLOOR, DEGRADE_MIN_THRESHOLD,
	};
	use base::base_types::{Entity, Kern};
	#[allow(unused_imports)]
	use base::base_types::{Reason, ReasonKind, ReviewState};
	#[allow(unused_imports)]
	use graph::reason::{add_reason, remove_entity, remove_reason};
	#[allow(unused_imports)]
	use math::{average_vec, reason_id};

	fn edge(from: &str, to: &str, score: f64) -> Reason {
		Reason {
			from: from.into(),
			to: to.into(),
			id: format!("{from}->{to}"),
			score,
			..Default::default()
		}
	}

	#[test]
	fn degrade_decays_survivors_and_removes_below_threshold() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		// BASE=0.15 pushes a->c (0.0) below the 0.05 floor; a->b (1.0) merely decays.
		add_reason(&mut k, edge("a", "b", 1.0));
		add_reason(&mut k, edge("a", "c", 0.0));
		g.kerns.insert("kx".into(), k);

		let (decayed, removed) = degrade_entity_reasons(&mut g, "kx", "a");

		assert_eq!(decayed, 2, "both incident edges visited");
		assert_eq!(removed, 1, "the sub-threshold edge is reaped");

		let kern = g.kerns.get("kx").expect("kern present");
		assert_eq!(kern.reasons.len(), 1, "only the healthy edge remains");
		let survivor = kern.reasons.get("a->b").expect("a->b survives");
		assert!(
			survivor.score < 1.0,
			"survivor was decayed, not left untouched"
		);
		assert!(
			survivor.score >= DEGRADE_MIN_THRESHOLD,
			"survivor stays above the floor"
		);
	}

	#[test]
	fn degrade_clamps_edge_score_at_floor() {
		// Under current constants DEGRADE_MIN_THRESHOLD (0.05) > DEGRADE_FLOOR (0.0),
		// so the threshold removes an edge before the clamp can fire on it. The
		// clamp is defensive: it holds the invariant "no surviving reason score
		// is below the floor" regardless of the threshold, and becomes live the
		// moment a score arrives below the floor (e.g. absorbing a pre-floor-era
		// value from disk) or the threshold is lowered. Pin the invariant.
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		// A survivor well above the threshold, plus a sub-threshold edge that is removed.
		add_reason(&mut k, edge("a", "b", 1.0));
		add_reason(&mut k, edge("a", "c", 0.0));
		g.kerns.insert("kx".into(), k);

		degrade_entity_reasons(&mut g, "kx", "a");

		let kern = g.kerns.get("kx").expect("kern present");
		for r in kern.reasons.values() {
			assert!(
				r.score >= DEGRADE_FLOOR,
				"surviving reason {} score {} is below the floor {}",
				r.id,
				r.score,
				DEGRADE_FLOOR
			);
		}
	}

	// `kern link` takes no writer lock, so a daemon can commit between its load
	// and its flush. The unguarded save writes the whole kern map with no epoch
	// check, so that commit vanishes — the last half of item 9 that needed no
	// auth to close.
	#[test]
	fn a_link_racing_an_external_commit_keeps_both() {
		use parking_lot::RwLock;
		use std::sync::Arc;

		use base::base_types::{mk_entity, EntityKind};

		let dir = tempfile::tempdir().unwrap();
		let cfg = config::Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};

		let g = Arc::new(RwLock::new(crate::load_graph(&cfg)));
		let root_id = g.read().root.id.clone();

		let mut own = Kern::new("link-kern", &root_id);
		for id in ["a", "b"] {
			own
				.entities
				.insert(id.into(), mk_entity(id, id, 1.0, EntityKind::Claim));
		}
		g.write().kerns.insert("link-kern".into(), own);
		crate::save_graph_guarded(&g, &cfg);

		// What `cmd_link` holds: loaded now, flushed only after the daemon commits.
		// That staleness is the defect — a graph loaded fresh would already carry
		// the other writer's kern and write it back by accident.
		let stale = crate::load_graph(&cfg);
		crate::test_helpers::commit_extra_kern_via_store(&g, Kern::new("daemon-kern", &root_id));
		drop(g);

		let linked = link_and_persist(stale, &cfg, "a", "b", "because".into(), None);
		assert!(linked.is_ok(), "the link itself applies: {linked:?}");

		let disk = crate::load_graph(&cfg);
		assert!(
			disk.loaded("daemon-kern").is_some(),
			"the concurrent writer's kern survived the link's flush"
		);
		let kern = disk.kerns.get("link-kern").expect("our own kern persisted");
		assert!(
			kern.reasons.values().any(|r| r.from == "a" && r.to == "b"),
			"the edge we just wrote is on disk too"
		);
	}

	#[test]
	fn degrade_on_unknown_kern_is_a_noop() {
		let mut g = GraphGnn::new();
		let (decayed, removed) = degrade_entity_reasons(&mut g, "missing", "a");
		assert_eq!((decayed, removed), (0, 0));
	}

	use base::base_types::EntityKind;

	fn ent(id: &str, kind: EntityKind) -> Entity {
		Entity {
			id: id.into(),
			kind,
			..Default::default()
		}
	}

	fn graph_with(entities: &[(&str, EntityKind)], edges: &[(&str, &str)]) -> GraphGnn {
		graph_in("kx", entities, edges)
	}

	fn graph_in(kern_id: &str, entities: &[(&str, EntityKind)], edges: &[(&str, &str)]) -> GraphGnn {
		let mut g = GraphGnn::new();
		let mut k = Kern::new(kern_id, "");
		for (id, kind) in entities {
			k.entities.insert((*id).into(), ent(id, *kind));
		}
		for (from, to) in edges {
			add_reason(&mut k, edge(from, to, 1.0));
		}
		g.register(k);
		g
	}

	#[test]
	fn forget_removes_thought_and_reports_edge_delta() {
		let mut g = graph_with(
			&[
				("a", EntityKind::Claim),
				("b", EntityKind::Claim),
				("c", EntityKind::Claim),
			],
			&[("a", "b"), ("a", "c")],
		);
		let removed = forget_entity(&mut g, "a", false).expect("non-fact forget succeeds");
		assert_eq!(removed, 2, "both incident edges went with a");
		let kern = g.kerns.get("kx").expect("kern present");
		assert!(!kern.entities.contains_key("a"), "a is gone from the kern");
		assert!(kern.entities.contains_key("b"), "neighbours survive");
	}

	#[test]
	fn forget_refuses_a_fact() {
		let mut g = graph_with(&[("f", EntityKind::Fact)], &[]);
		assert_eq!(
			forget_entity(&mut g, "f", false),
			Err("cannot forget a fact")
		);
		assert!(
			g.kerns.get("kx").unwrap().entities.contains_key("f"),
			"the fact is left intact"
		);
	}

	#[test]
	fn forget_unknown_id_is_rejected_not_panicked() {
		let mut g = graph_with(&[("a", EntityKind::Claim)], &[]);
		assert_eq!(
			forget_entity(&mut g, "nope", false),
			Err("thought not found")
		);
	}

	fn file_src(path: &str, section: &str) -> Source {
		Source::File {
			path: path.into(),
			section: section.into(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		}
	}

	fn sourced(id: &str, kind: EntityKind, source: Source) -> Entity {
		Entity {
			id: id.into(),
			kind,
			source,
			..Default::default()
		}
	}

	fn graph_of(kerns: &[(&str, Vec<Entity>)]) -> GraphGnn {
		let mut g = GraphGnn::new();
		for (kern_id, entities) in kerns {
			let mut k = Kern::new(*kern_id, "");
			for e in entities {
				k.entities.insert(e.id.clone(), e.clone());
			}
			g.register(k);
		}
		g
	}

	// The point of keying on (scheme, object_id) and NOT source_id: source_id
	// hashes the section too, so a per-section key would forget one chunk of a
	// document and silently leave the rest.
	#[test]
	fn forget_by_source_takes_every_section_of_one_object() {
		let mut g = graph_of(&[(
			"kx",
			vec![
				sourced("intro", EntityKind::Claim, file_src("notes.md", "intro")),
				sourced("body", EntityKind::Claim, file_src("notes.md", "body")),
				sourced(
					"other",
					EntityKind::Claim,
					file_src("elsewhere.md", "intro"),
				),
			],
		)]);
		add_reason(g.kerns.get_mut("kx").unwrap(), edge("intro", "body", 1.0));

		let out = forget_by_source(&mut g, "file", "notes.md", false);

		assert_eq!(out.removed_entities, 2, "both sections went");
		assert_eq!(out.removed_edges, 1, "the edge between them cascaded");
		assert_eq!(out.kept_facts, 0);
		let kern = g.kerns.get("kx").expect("kern present");
		assert!(!kern.entities.contains_key("intro"));
		assert!(!kern.entities.contains_key("body"));
		assert!(
			kern.entities.contains_key("other"),
			"a different object_id is untouched"
		);
	}

	// A source's chunks do not all live in one kern — placement is by similarity,
	// so a scan of a single kern would leave half the document behind.
	#[test]
	fn forget_by_source_reaches_across_kerns() {
		let mut g = graph_of(&[
			(
				"k1",
				vec![sourced(
					"a",
					EntityKind::Claim,
					file_src("notes.md", "intro"),
				)],
			),
			(
				"k2",
				vec![sourced(
					"b",
					EntityKind::Claim,
					file_src("notes.md", "body"),
				)],
			),
		]);

		let out = forget_by_source(&mut g, "file", "notes.md", false);

		assert_eq!(out.removed_entities, 2, "both kerns were swept");
		assert!(!g.kerns.get("k1").unwrap().entities.contains_key("a"));
		assert!(!g.kerns.get("k2").unwrap().entities.contains_key("b"));
	}

	// The whole reason `force` exists — and the reason it must never be the
	// default: without it a legal source deletion leaves the Facts behind, and
	// the caller has to be told that is what happened.
	#[test]
	fn a_local_fact_survives_without_force_and_goes_with_it() {
		let entities = || {
			vec![
				sourced("f", EntityKind::Fact, file_src("notes.md", "intro")),
				sourced("c", EntityKind::Claim, file_src("notes.md", "body")),
			]
		};

		let mut g = graph_of(&[("kx", entities())]);
		let out = forget_by_source(&mut g, "file", "notes.md", false);
		assert_eq!(out.removed_entities, 1, "only the Claim went");
		assert_eq!(out.kept_facts, 1, "the refusal is reported, not swallowed");
		assert!(
			g.kerns.get("kx").unwrap().entities.contains_key("f"),
			"the local Fact is actually still there, not just reported kept"
		);

		let mut g = graph_of(&[("kx", entities())]);
		let out = forget_by_source(&mut g, "file", "notes.md", true);
		assert_eq!(out.removed_entities, 2, "force took the Fact too");
		assert_eq!(out.kept_facts, 0);
		assert!(
			!g.kerns.get("kx").unwrap().entities.contains_key("f"),
			"force removes the local Fact for real — remove_entity guards it too"
		);
	}

	// Deleting a source the graph never ingested is a legal no-op, not an error:
	// the host deletes what it has, and kern reports what it had.
	#[test]
	fn an_unknown_source_removes_nothing_and_does_not_error() {
		let mut g = graph_of(&[(
			"kx",
			vec![sourced(
				"a",
				EntityKind::Claim,
				file_src("notes.md", "intro"),
			)],
		)]);

		let out = forget_by_source(&mut g, "file", "never-ingested.md", false);
		assert_eq!((out.removed_entities, out.removed_edges), (0, 0));
		assert_eq!(out.kept_facts, 0);
		assert!(
			g.kerns.get("kx").unwrap().entities.contains_key("a"),
			"a miss must not take the graph with it"
		);

		// Same object_id under another scheme is a different source.
		let inline = forget_by_source(&mut g, "inline", "notes.md", false);
		assert_eq!(inline.removed_entities, 0, "the scheme is half the key");
	}

	#[test]
	fn the_source_selector_is_scheme_colon_slash_slash_object_id() {
		assert_eq!(
			parse_source_selector("file:///abs/path/notes.md"),
			Ok(("file", "/abs/path/notes.md")),
			"everything after :// is the object_id, slashes and all"
		);
		assert_eq!(
			parse_source_selector("inline://deadbeef"),
			Ok(("inline", "deadbeef"))
		);

		for bad in ["notes.md", "file://", "ftp://x", "://x"] {
			assert!(
				parse_source_selector(bad).is_err(),
				"{bad} must be rejected, not guessed at"
			);
		}
	}

	// Proves the printer, not the lookup: both `kern get` paths hand it this shape,
	// so the labels and the edge direction must come back out of the JSON intact.
	#[test]
	fn detail_json_carries_everything_the_get_printer_needs() {
		let mut g = graph_with(
			&[("a", EntityKind::Question), ("b", EntityKind::Claim)],
			&[("a", "b")],
		);
		{
			let a = g
				.kerns
				.get_mut("kx")
				.unwrap()
				.entities
				.get_mut("a")
				.unwrap();
			a.set_text("the question".into());
			a.source = base::base_types::Source::Session {
				session_id: "session:sess-1".into(),
				section: "2,5".into(),
				title: String::new(),
			};
		}
		let v = entity_detail_by_id(&g, "a").expect("a resolves");

		assert_eq!(entity_kind_label(u64_field(&v, "kind")), "Question");
		assert_eq!(str_field(&v, "text"), "the question");
		assert_eq!(str_field(&v, "kern"), "kx");
		let src = v.get("source").expect("detail carries provenance");
		assert_eq!(str_field(src, "scheme"), "session");
		assert_eq!(str_field(src, "object_id"), "session:sess-1");
		assert_eq!(str_field(src, "section"), "2,5");
		let edges = array_field(&v, "edges");
		assert_eq!(edges.len(), 1);
		assert_eq!(str_field(&edges[0], "from"), "a", "edge points outward");
		assert_eq!(
			reason_kind_label(u64_field(&edges[0], "kind")),
			"Similarity"
		);
		assert!(entity_detail_by_id(&g, "nope").is_none());
	}

	// The id path has no `drop_expired` in front of it and never will — it answers
	// a named row, not "what is true now". The ranked path cannot satisfy this
	// test: it is not involved.
	#[test]
	fn the_id_path_flags_an_expired_thought_instead_of_hiding_it() {
		use std::time::{Duration, SystemTime, UNIX_EPOCH};
		let now = SystemTime::now();
		let mut g = graph_with(
			&[
				("dead", EntityKind::Fact),
				("live", EntityKind::Fact),
				("forever", EntityKind::Fact),
			],
			&[],
		);
		let deadline = now - Duration::from_secs(3600);
		let k = g.kerns.get_mut("kx").expect("kern present");
		k.entities.get_mut("dead").unwrap().valid_until = Some(deadline);
		k.entities.get_mut("live").unwrap().valid_until = Some(now + Duration::from_secs(3600));

		let dead = entity_detail_by_id(&g, "dead").expect(
			"an expired thought still resolves by id — 'not found' would lie about a \
			 row GC never collects",
		);
		assert_eq!(dead["expired"], serde_json::json!(true));
		assert_eq!(
			dead["valid_until"],
			serde_json::json!(deadline.duration_since(UNIX_EPOCH).unwrap().as_secs()),
			"the deadline travels with the flag so the caller can judge the staleness"
		);

		let live = entity_detail_by_id(&g, "live").expect("live resolves");
		assert_eq!(live["expired"], serde_json::json!(false));

		let no_ttl = entity_detail_by_id(&g, "forever").expect("no-retention thought resolves");
		assert!(
			no_ttl.get("expired").is_none() && no_ttl.get("valid_until").is_none(),
			"no retention means no keys at all, not expired=false: {no_ttl}"
		);
	}
}
