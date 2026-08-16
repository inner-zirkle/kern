//! Tests extracted from mcp_tools_mutate.rs
#![allow(unused)]
use super::*;

mod tests {
	use crate::server::Server;
	use base::base_types::{Entity, EntityKind, Kern, Reason};
	use graph::reason::add_reason;

	fn make_server() -> Server {
		crate::test_helpers::server()
	}

	fn insert_kern(srv: &Server, kern: Kern) {
		srv.graph.write().kerns.insert(kern.id.clone(), kern);
	}

	#[tokio::test]
	async fn tool_forget_removes_entity_and_counts_cascaded_edges() {
		let srv = make_server();
		let mut k = Kern::new("kx", "");
		k.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				..Default::default()
			},
		);
		k.entities.insert(
			"b".into(),
			Entity {
				id: "b".into(),
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				id: "a->b".into(),
				from: "a".into(),
				to: "b".into(),
				..Default::default()
			},
		);
		insert_kern(&srv, k);

		let out = srv.tool_forget(&serde_json::json!({ "id": "a" }));
		assert!(out.is_ok());
		assert_eq!(
			out.unwrap()["removed_edges"],
			1,
			"the incident edge cascades"
		);

		let g = srv.graph.read();
		assert!(
			!g.kerns.get("kx").unwrap().entities.contains_key("a"),
			"entity is gone"
		);
	}

	#[tokio::test]
	async fn tool_forget_refuses_a_fact() {
		let srv = make_server();
		let mut k = Kern::new("kx", "");
		k.entities.insert(
			"f".into(),
			Entity {
				id: "f".into(),
				kind: EntityKind::Fact,
				..Default::default()
			},
		);
		insert_kern(&srv, k);

		let out = srv.tool_forget(&serde_json::json!({ "id": "f" }));
		assert!(out.is_err());
		assert!(out.unwrap_err().contains("cannot forget a fact"));
	}

	fn sourced(id: &str, kind: EntityKind, path: &str, section: &str) -> Entity {
		Entity {
			id: id.into(),
			kind,
			source: base::base_types::Source::File {
				path: path.into(),
				section: section.into(),
				title: String::new(),
				author: String::new(),
				url: String::new(),
			},
			..Default::default()
		}
	}

	fn source_server() -> Server {
		let srv = make_server();
		let mut k = Kern::new("kx", "");
		for e in [
			sourced("intro", EntityKind::Claim, "notes.md", "intro"),
			sourced("body", EntityKind::Claim, "notes.md", "body"),
			sourced("pinned", EntityKind::Fact, "notes.md", "pinned"),
			sourced("other", EntityKind::Claim, "elsewhere.md", ""),
		] {
			k.entities.insert(e.id.clone(), e);
		}
		add_reason(
			&mut k,
			Reason {
				id: "intro->body".into(),
				from: "intro".into(),
				to: "body".into(),
				..Default::default()
			},
		);
		insert_kern(&srv, k);
		srv
	}

	// Repo law 3: one dispatcher. `kern forget --source` routes by tool NAME over
	// the socket, so a handler that exists but is not reachable through
	// `invoke` answers "unknown tool" and sends the CLI back to writing the
	// store behind the daemon — exactly what item 19 must not do.
	#[tokio::test]
	async fn forget_by_source_dispatches_through_invoke() {
		let srv = source_server();
		let body = srv
			.invoke(
				"forget_by_source",
				&serde_json::json!({"scheme": "file", "object_id": "notes.md"}),
			)
			.expect("the dispatcher answers");
		assert_eq!(body["removed_entities"], 2, "both Claim sections went");
		assert_eq!(body["removed_edges"], 1, "the edge between them cascaded");
		assert_eq!(body["kept_facts"], 1, "the Fact was refused, and said so");

		let g = srv.graph.read();
		let kern = g.kerns.get("kx").unwrap();
		assert!(!kern.entities.contains_key("intro"));
		assert!(kern.entities.contains_key("pinned"), "Fact untouched");
		assert!(
			kern.entities.contains_key("other"),
			"other source untouched"
		);
	}

	#[tokio::test]
	async fn tool_forget_by_source_force_takes_the_local_fact() {
		let srv = source_server();
		let out = srv.tool_forget_by_source(
			&serde_json::json!({"scheme": "file", "object_id": "notes.md", "force": true}),
		);
		assert!(out.is_ok(), "{out:?}");
		let body = out.unwrap();
		assert_eq!(body["removed_entities"], 3);
		assert_eq!(body["kept_facts"], 0);
		assert!(
			!srv
				.graph
				.read()
				.kerns
				.get("kx")
				.unwrap()
				.entities
				.contains_key("pinned"),
			"force is the one bypass and it has to actually bite"
		);
	}

	#[tokio::test]
	async fn tool_forget_by_source_rejects_an_unknown_scheme() {
		let srv = source_server();
		let out =
			srv.tool_forget_by_source(&serde_json::json!({"scheme": "ftp", "object_id": "notes.md"}));
		assert!(out.is_err());
		let msg = out.unwrap_err();
		assert!(msg.contains("unknown source scheme"), "{msg}");

		// An unknown *object* is a legal no-op — only the scheme is a caller error.
		let out = srv.tool_forget_by_source(
			&serde_json::json!({"scheme": "file", "object_id": "never-ingested.md"}),
		);
		assert!(out.is_ok(), "{out:?}");
		assert_eq!(out.unwrap()["removed_entities"], 0);
	}

	#[tokio::test]
	async fn tool_degrade_decays_survivors_and_reaps_subthreshold() {
		let srv = make_server();
		let mut k = Kern::new("kx", "");
		k.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				id: "a->b".into(),
				from: "a".into(),
				to: "b".into(),
				score: 1.0,
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				id: "a->c".into(),
				from: "a".into(),
				to: "c".into(),
				score: 0.0,
				..Default::default()
			},
		);
		insert_kern(&srv, k);

		let out = srv.tool_degrade(&serde_json::json!({ "query_id": "a" }));
		assert!(out.is_ok());
		assert_eq!(
			out.unwrap()["decayed_edges"],
			2,
			"both incident edges visited"
		);

		let g = srv.graph.read();
		let kern = g.kerns.get("kx").unwrap();
		assert_eq!(kern.reasons.len(), 1, "the sub-threshold edge is reaped");
		let r = kern.reasons.get("a->b").expect("the healthy edge survives");
		assert!(r.score_lamport > 0, "decay stamped for the LWW join");
		assert!(
			!r.score_producer.is_empty(),
			"and carries the replica that stamped it"
		);
	}

	#[tokio::test]
	async fn tool_link_adds_edge_with_provided_reason_text() {
		let srv = make_server();
		let mut k = Kern::new("kx", "");
		k.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			},
		);
		k.entities.insert(
			"b".into(),
			Entity {
				id: "b".into(),
				vector: vec![0.0, 1.0].into(),
				..Default::default()
			},
		);
		insert_kern(&srv, k);

		let out =
			srv.tool_link(&serde_json::json!({ "from": "a", "to": "b", "reason": "because related" }));
		assert!(out.is_ok());
		let edge_id = out.unwrap()["edge_id"]
			.as_str()
			.expect("edge_id")
			.to_string();

		let g = srv.graph.read();
		let r = g
			.kerns
			.get("kx")
			.unwrap()
			.reasons
			.get(&edge_id)
			.expect("edge added to from-kern");
		assert_eq!(
			r.text, "because related",
			"provided reason used verbatim (no LLM configured)"
		);
		assert_eq!((r.from.as_str(), r.to.as_str()), ("a", "b"));
	}

	#[tokio::test]
	async fn tool_link_errors_on_unknown_endpoint() {
		let srv = make_server();
		let out = srv.tool_link(&serde_json::json!({ "from": "nope", "to": "nada", "reason": "x" }));
		assert!(out.is_err());
		assert!(out.unwrap_err().contains("not found"));
	}

	fn move_server() -> Server {
		let srv = make_server();
		let mut src = Kern::new("src", "");
		src.entities.insert("a".into(), test_support::entity("a"));
		src.entities.insert("b".into(), test_support::entity("b"));
		add_reason(&mut src, test_support::edge("a", "b"));
		insert_kern(&srv, src);
		insert_kern(&srv, Kern::new("dst", ""));
		srv
	}

	#[tokio::test]
	async fn tool_move_carries_entity_and_outgoing_edges() {
		let srv = move_server();

		let out = srv.tool_move(&serde_json::json!({ "id": "a", "to_kern": "dst" }));
		assert!(out.is_ok(), "{out:?}");
		let body = out.unwrap();
		assert_eq!(body["from_kern"], "src");
		assert_eq!(body["to_kern"], "dst");

		let g = srv.graph.read();
		let src = g.kerns.get("src").unwrap();
		let dst = g.kerns.get("dst").unwrap();
		assert!(dst.entities.contains_key("a"), "entity relocated");
		assert!(!src.entities.contains_key("a"), "entity left src");
		let moved = dst.reasons.get("a->b").expect("outgoing edge travelled");
		assert_eq!(
			moved.to_kern_id, "src",
			"target b stayed behind, so the edge is stamped cross-kern"
		);
		assert!(!src.reasons.contains_key("a->b"));
	}

	#[tokio::test]
	async fn tool_move_rejects_unknown_entity_and_unknown_destination() {
		let srv = move_server();

		let out = srv.tool_move(&serde_json::json!({ "id": "ghost", "to_kern": "dst" }));
		assert!(out.is_err());
		let msg = out.unwrap_err();
		assert!(msg.contains("thought not found"), "{msg}");

		let out = srv.tool_move(&serde_json::json!({ "id": "a", "to_kern": "ghost_kern" }));
		assert!(out.is_err());
		let msg = out.unwrap_err();
		assert!(msg.contains("kern not found"), "{msg}");

		// The rejected destination must not have cost us the entity.
		let g = srv.graph.read();
		let src = g.kerns.get("src").unwrap();
		assert!(src.entities.contains_key("a"), "entity survives a bad move");
		assert!(src.reasons.contains_key("a->b"), "edge survives a bad move");
	}

	#[tokio::test]
	async fn tool_move_rejects_malformed_arguments() {
		let srv = move_server();
		let out = srv.tool_move(&serde_json::json!({ "id": "a" }));
		assert!(out.is_err());
		let msg = out.unwrap_err();
		assert!(msg.contains("invalid arguments"), "{msg}");
	}

	// The release half of item 21. Reading the row back matters: a `promote` that
	// answered `promoted: true` without touching the entity would satisfy every
	// assertion that only inspects the envelope.
	#[tokio::test]
	async fn tool_promote_releases_a_held_row_and_is_idempotent() {
		use base::base_types::{ReviewState, Source};

		let srv = make_server();
		let mut k = Kern::new("kx", "");
		k.entities.insert(
			"held".into(),
			Entity {
				id: "held".into(),
				kind: EntityKind::Claim,
				review: ReviewState::Pending,
				source: Source::Inline {
					hash: "h".into(),
					section: String::new(),
				},
				statements: vec!["an uncurated claim".into()],
				..Default::default()
			},
		);
		insert_kern(&srv, k);

		let review = || srv.graph.read().kerns["kx"].entities["held"].review;
		assert_eq!(review(), ReviewState::Pending, "precondition: it is held");

		let out = srv.tool_promote(&serde_json::json!({"id": "held"}));
		assert!(out.is_ok(), "{out:?}");
		assert_eq!(out.unwrap()["promoted"], serde_json::json!(true));
		assert_eq!(
			review(),
			ReviewState::Active,
			"the row itself moved, not just the answer"
		);

		// Again: a success that changed nothing, never an error — a curator who
		// retries must not be told the release failed.
		let out = srv.tool_promote(&serde_json::json!({"id": "held"}));
		assert!(out.is_ok(), "{out:?}");
		assert_eq!(out.unwrap()["promoted"], serde_json::json!(false));
		assert_eq!(review(), ReviewState::Active);
	}

	#[tokio::test]
	async fn tool_promote_refuses_an_unknown_id_rather_than_reporting_success() {
		let srv = make_server();
		let out = srv.tool_promote(&serde_json::json!({"id": "ghost"}));
		assert!(
			out.is_err(),
			"a mistyped id must not read back as a released claim: {out:?}"
		);
		let msg = out.unwrap_err();
		assert!(
			msg.contains("thought not found"),
			"names what failed: {msg}"
		);
	}
}
