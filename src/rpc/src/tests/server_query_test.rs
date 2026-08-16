//! Tests extracted from mcp_tools_query.rs
#![allow(unused)]
use super::*;

mod envelope_shape_tests {
	use base::base_types::{ChunkPart, ChunkPartKind, Entity, EntityKind, EntityStatus, Source};
	use retrieval::id_detail::base_entity_json as build_entity_json;

	fn entity_with(kind: EntityKind, status: EntityStatus, source: Source) -> Entity {
		Entity {
			id: "e1".into(),
			kind,
			status,
			source,
			statements: vec!["hello world".into()],
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		}
	}

	#[test]
	fn envelope_includes_kind_scheme_status_for_active_entity() {
		let ent = entity_with(
			EntityKind::Fact,
			EntityStatus::Active,
			Source::File {
				path: "src/main.rs".into(),
				section: "fn main".into(),
				title: String::new(),
				author: String::new(),
				url: "https://example.test/src/main.rs".into(),
			},
		);
		let v = build_entity_json(&ent, 0.5);
		assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("fact"));
		assert_eq!(v.get("scheme").and_then(|x| x.as_str()), Some("file"));
		assert_eq!(v.get("status").and_then(|x| x.as_str()), Some("active"));
		// The ranked envelope carries the full source backlink, not just the scheme
		// it matched on, so a caller can open the proving page.
		let source = v.get("source").expect("envelope carries a source backlink");
		assert_eq!(source.get("scheme").and_then(|x| x.as_str()), Some("file"));
		assert_eq!(
			source.get("object_id").and_then(|x| x.as_str()),
			Some("src/main.rs")
		);
		assert_eq!(
			source.get("section").and_then(|x| x.as_str()),
			Some("fn main")
		);
		assert_eq!(
			source.get("url").and_then(|x| x.as_str()),
			Some("https://example.test/src/main.rs")
		);
	}

	#[test]
	fn envelope_status_is_superseded_when_entity_superseded() {
		let ent = entity_with(
			EntityKind::Claim,
			EntityStatus::Superseded,
			Source::Inline {
				hash: "h".into(),
				section: String::new(),
			},
		);
		let v = build_entity_json(&ent, 0.0);
		assert_eq!(v.get("status").and_then(|x| x.as_str()), Some("superseded"));
		assert_eq!(v.get("scheme").and_then(|x| x.as_str()), Some("inline"));
		assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some("claim"));
		// A scheme with no url still carries the backlink block, with object_id set
		// to what `Source::object_id` returns for it (the inline hash).
		let source = v.get("source").expect("envelope carries a source backlink");
		assert_eq!(
			source.get("scheme").and_then(|x| x.as_str()),
			Some("inline")
		);
		assert_eq!(source.get("object_id").and_then(|x| x.as_str()), Some("h"));
		assert_eq!(source.get("url").and_then(|x| x.as_str()), Some(""));
	}

	#[test]
	fn envelope_emits_every_kind_label() {
		for k in [
			EntityKind::Fact,
			EntityKind::Claim,
			EntityKind::Document,
			EntityKind::Question,
			EntityKind::Conclusion,
		] {
			let ent = entity_with(k, EntityStatus::Active, Source::default());
			let v = build_entity_json(&ent, 0.0);
			assert_eq!(v.get("kind").and_then(|x| x.as_str()), Some(k.as_str()));
		}
	}
}
mod id_filter_tests {
	use crate::server::Server;
	use base::base_types::{Entity, EntityKind, Kern, Source};

	fn server_with(thought: Entity) -> Server {
		let srv = crate::test_helpers::server();
		let mut k = Kern::new("kx", "");
		k.entities.insert(thought.id.clone(), thought);
		srv.graph.write().kerns.insert("kx".into(), k);
		srv
	}

	fn fact(id: &str) -> Entity {
		Entity {
			id: id.into(),
			kind: EntityKind::Fact,
			source: Source::Inline {
				hash: "h".into(),
				section: String::new(),
			},
			statements: vec!["a settled thing".into()],
			..Default::default()
		}
	}

	#[tokio::test]
	async fn id_read_drops_a_row_the_kind_filter_excludes() {
		let srv = server_with(fact("f1"));
		let out = srv.tool_query(&serde_json::json!({"id": "f1", "kind": "claim"}));
		assert!(
			out.is_err(),
			"a Fact must not survive kind=claim just because it was named by id: {out:?}"
		);
		assert!(out.unwrap_err().contains("thought not found"));
	}

	#[tokio::test]
	async fn id_read_keeps_a_row_the_filters_admit() {
		let srv = server_with(fact("f1"));
		let out = srv.tool_query(&serde_json::json!({
			"id": "f1", "kind": "fact", "scheme": "inline",
		}));
		assert!(out.is_ok(), "matching filters must not hide it: {out:?}");
		assert_eq!(out.unwrap()["id"], serde_json::json!("f1"));
	}

	#[tokio::test]
	async fn id_read_reports_a_bad_filter_rather_than_ignoring_it() {
		let srv = server_with(fact("f1"));
		let out = srv.tool_query(&serde_json::json!({"id": "f1", "since": "not-a-time"}));
		assert!(out.is_err());
		let msg = out.unwrap_err();
		assert!(msg.contains("since"), "names the field: {msg}");
	}

	fn distilled_claim(id: &str, label: &str) -> Entity {
		Entity {
			id: id.into(),
			kind: EntityKind::Claim,
			source: Source::Session {
				session_id: "session:x".into(),
				section: String::new(),
				title: format!("session://{label}"),
			},
			statements: vec!["a distilled thing".into()],
			..Default::default()
		}
	}

	// The subClassOf half of the claim_kind filter: a sub-kind registered under
	// a parent must answer a query that filtered on the parent.
	#[tokio::test]
	async fn claim_kind_filter_admits_registered_sub_kinds_of_the_asked_parent() {
		let srv = server_with(distilled_claim("c1", "rust-fact"));
		srv
			.graph
			.write()
			.root
			.add_claim_kind(
				"rust-fact",
				"rust facts",
				Some("code-fact"),
				&ingest::distill::DEFAULT_KINDS,
			)
			.expect("builtin parent registers");

		let out = srv.tool_query(&serde_json::json!({"id": "c1", "claim_kind": "code-fact"}));
		assert!(
			out.is_ok(),
			"a rust-fact claim answers a code-fact filter through the hierarchy: {out:?}"
		);
		assert_eq!(out.unwrap()["id"], serde_json::json!("c1"));

		let out = srv.tool_query(&serde_json::json!({"id": "c1", "claim_kind": "preference"}));
		assert!(
			out.is_err(),
			"an unrelated claim kind must not match: {out:?}"
		);
	}

	#[tokio::test]
	async fn claim_kind_filter_rejects_an_unknown_label_instead_of_matching_nothing() {
		let srv = server_with(distilled_claim("c1", "decision"));
		let out = srv.tool_query(&serde_json::json!({"id": "c1", "claim_kind": "ghost"}));
		assert!(out.is_err());
		let msg = out.unwrap_err();
		assert!(msg.contains("unknown claim kind"), "a typo says so: {msg}");
	}

	// The retired item 91 decision: retention on the id surface annotates, it does
	// not hide. Filtering the id read must not smuggle `drop_expired` in behind it
	// — an unfiltered `QueryOptions` leaves `valid_at`/`as_of` off, so the expired
	// row still arrives, flagged.
	// Batch direct lookup: `ids` returns one detail per found id and lists the
	// rest under `missing`. Each id honours the same filters `id` does.
	#[tokio::test]
	async fn batch_ids_returns_found_and_missing() {
		let mut k = Kern::new("kx", "");
		k.entities.insert("f1".into(), fact("f1"));
		k.entities.insert("f2".into(), fact("f2"));
		let srv = crate::test_helpers::server();
		srv.graph.write().kerns.insert("kx".into(), k);

		let out = srv.tool_query(&serde_json::json!({"ids": ["f1", "f2", "ghost"]}));
		assert!(out.is_ok(), "batch is not an error: {out:?}");
		let v = out.unwrap();
		let results = v["results"].as_array().expect("results array");
		let missing = v["missing"].as_array().expect("missing array");
		assert_eq!(results.len(), 2, "two found: {v}");
		assert_eq!(missing.len(), 1, "only ghost is missing: {v}");
		assert_eq!(
			missing[0],
			serde_json::json!("ghost"),
			"ghost is missing: {v}"
		);
		let ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
		assert!(
			ids.contains(&"f1") && ids.contains(&"f2"),
			"both ids in results: {ids:?}"
		);
	}

	// A filter on a batch id read drops the non-matching row into `missing` —
	// the same per-row predicate the single-id path uses, not a silent skip.
	#[tokio::test]
	async fn batch_ids_filter_drops_non_matching_into_missing() {
		let mut k = Kern::new("kx", "");
		k.entities.insert("f1".into(), fact("f1"));
		let mut claim = fact("c1");
		claim.kind = EntityKind::Claim;
		k.entities.insert("c1".into(), claim);
		let srv = crate::test_helpers::server();
		srv.graph.write().kerns.insert("kx".into(), k);

		let out = srv.tool_query(&serde_json::json!({"ids": ["f1", "c1"], "kind": "fact"}));
		let v = out.unwrap();
		let results = v["results"].as_array().expect("results");
		let missing = v["missing"].as_array().expect("missing");
		assert_eq!(results.len(), 1, "only the fact passes the filter: {v}");
		assert_eq!(results[0]["id"], serde_json::json!("f1"));
		assert_eq!(missing.len(), 1, "only the claim is missing: {v}");
		assert_eq!(
			missing[0],
			serde_json::json!("c1"),
			"the claim is filtered out: {v}"
		);
	}

	#[tokio::test]
	async fn bare_id_read_still_serves_an_expired_row_flagged() {
		let mut e = fact("f1");
		let deadline = util::parse_rfc3339("2020-01-01T00:00:00Z").expect("fixed ts");
		e.valid_until = Some(deadline);
		let srv = server_with(e);

		let out = srv.tool_query(&serde_json::json!({"id": "f1"}));
		assert!(
			out.is_ok(),
			"'not found' would lie about a row that is demonstrably on disk: {out:?}"
		);
		let v = out.unwrap();
		assert_eq!(v["expired"], serde_json::json!(true));
		assert!(
			v.get("valid_until").is_some(),
			"deadline travels with the flag"
		);

		// Ask for validity explicitly and it is a filter again, like any other.
		let out = srv.tool_query(&serde_json::json!({
			"id": "f1", "valid_at": "2026-01-01T00:00:00Z",
		}));
		assert!(out.is_err(), "an explicit valid_at does filter: {out:?}");
	}

	// Item 21's hold half, through the surface rather than the engine. Both
	// directions, for the reason `exclude_pending_drops_only_the_uncurated_and_
	// only_when_asked` gives: a held row that is never in the set proves nothing.
	#[tokio::test]
	async fn id_read_withholds_a_held_row_only_when_exclude_pending_is_asked() {
		let mut e = fact("f1");
		e.review = base::base_types::ReviewState::Pending;
		let srv = server_with(e);

		let out = srv.tool_query(&serde_json::json!({"id": "f1"}));
		assert!(
			out.is_ok(),
			"opt-in: nobody asked, so the held row still arrives: {out:?}"
		);
		assert_eq!(out.unwrap()["id"], serde_json::json!("f1"));

		let out = srv.tool_query(&serde_json::json!({"id": "f1", "exclude_pending": true}));
		assert!(
			out.is_err(),
			"a caller that asked to exclude pending must not be served one: {out:?}"
		);
		assert!(out.unwrap_err().contains("thought not found"));
	}
}
mod cold_tier_filter_tests {
	use base::base_types::{Entity, EntityKind, Source};

	fn spilled(id: &str, kind: EntityKind) -> Entity {
		let mut e = Entity {
			id: id.into(),
			kind,
			source: Source::Inline {
				hash: "h".into(),
				section: String::new(),
			},
			statements: vec![format!("cold statement {id}")],
			..Default::default()
		};
		e.vector = vec![1.0, 0.0, 0.0].into();
		e
	}

	// The cold tier is a raw cosine scan that answers no predicate of its own.
	// Filling the ranked read from it unfiltered made spilling an entity the way
	// around every filter the hot path enforces.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn a_cold_hit_answers_the_same_filter_the_hot_path_does() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|| async {
				axum::Json(serde_json::json!({ "embeddings": [[1.0, 0.0, 0.0]] }))
			}),
		);
		let (url, _server) = test_support::spawn_http(app).await;
		let mut srv = crate::test_helpers::server_with_embed_url(&url);
		// The ranked path embeds the query itself, so this rig needs the server's
		// own client, not just the worker's.
		srv.llm = Some(llm::Client::new_embed_only(&url, "test", ""));

		let dir = tempfile::tempdir().expect("tmpdir");
		let store = store::Store::open(&dir.path().to_string_lossy()).expect("store");
		store
			.cold_put_all(&[
				spilled("cold_fact", EntityKind::Fact),
				spilled("cold_claim", EntityKind::Claim),
			])
			.expect("spill");
		srv.graph.write().set_store(std::sync::Arc::new(store));

		let ids = |out: &Result<serde_json::Value, String>| -> Vec<String> {
			let body = out.as_ref().expect("json body");
			body["entities"]
				.as_array()
				.cloned()
				.unwrap_or_default()
				.iter()
				.filter_map(|e| e["id"].as_str().map(str::to_string))
				.collect()
		};

		// Precondition: no filter, so both cold rows arrive.
		let out = srv.tool_query(&serde_json::json!({"text": "anything"}));
		assert!(out.is_ok(), "{out:?}");
		let all = ids(&out);
		assert!(
			all.contains(&"cold_fact".to_string()) && all.contains(&"cold_claim".to_string()),
			"precondition: the cold fill reaches both rows: {all:?}"
		);

		let out = srv.tool_query(&serde_json::json!({"text": "anything", "kind": "fact"}));
		assert!(out.is_ok(), "{out:?}");
		let got = ids(&out);
		assert!(
			got.contains(&"cold_fact".to_string()),
			"the matching cold row still arrives: {got:?}"
		);
		assert!(
			!got.contains(&"cold_claim".to_string()),
			"a cold row must not dodge the kind filter just because it was spilled: {got:?}"
		);
	}
}
mod time_filter_tests {
	use super::parse_time_filter;

	#[test]
	fn empty_is_no_filter() {
		assert_eq!(parse_time_filter("since", "").unwrap(), None);
	}

	#[test]
	fn valid_parses_to_some() {
		assert!(parse_time_filter("before", "2026-06-05T09:00:00Z")
			.unwrap()
			.is_some());
	}

	#[test]
	fn nonempty_malformed_is_hard_error() {
		let e = parse_time_filter("valid_at", "20XX-06-05T09:00:00Z").unwrap_err();
		assert!(e.contains("valid_at"), "error names the field: {e}");
	}
}
mod exclude_pending_surface_tests {
	use super::{build_query_options, QueryArgs};

	fn args(v: serde_json::Value) -> QueryArgs {
		serde_json::from_value(v).expect("arguments deserialize")
	}

	#[test]
	fn exclude_pending_reaches_query_options_only_when_asked() {
		let opts = build_query_options(&args(serde_json::json!({"text": "x"}))).expect("options");
		assert!(
			!opts.exclude_pending,
			"opt-in: absent means no filter, so an uncurated graph reads as before"
		);
		assert!(
			!opts.is_active(),
			"and nothing forces the pre-filtered path"
		);

		let opts = build_query_options(&args(
			serde_json::json!({"text": "x", "exclude_pending": true}),
		))
		.expect("options");
		assert!(
			opts.exclude_pending,
			"the schema field has to land on the engine flag"
		);
		assert!(
			opts.is_active(),
			"an exclude_pending-only query takes the pre-filtered ANN path"
		);
	}
}
