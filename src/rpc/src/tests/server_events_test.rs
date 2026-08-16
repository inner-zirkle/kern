//! Tests extracted from mcp_tools_events.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::EventCursor;
	use crate::server::Server;
	use base::base_types::{Entity, EntityKind, EntityStatus, Kern, Source};
	use std::time::{Duration, UNIX_EPOCH};

	fn at(secs: u64) -> std::time::SystemTime {
		UNIX_EPOCH + Duration::from_secs(secs)
	}

	// A resident entity with a created stamp; superseded ones also carry
	// invalidated_at and a flipped status, exactly as the supersede path leaves
	// them (base/accept.rs stamp_superseded).
	fn ent(id: &str, created: u64) -> Entity {
		Entity {
			id: id.into(),
			kind: EntityKind::Claim,
			source: Source::Inline {
				hash: id.into(),
				section: String::new(),
			},
			statements: vec![format!("statement {id}")],
			created_at: Some(at(created)),
			..Default::default()
		}
	}

	fn superseded(mut e: Entity, invalidated: u64) -> Entity {
		e.status = EntityStatus::Superseded;
		e.invalidated_at = Some(at(invalidated));
		e
	}

	fn server_with(entities: Vec<Entity>) -> Server {
		let srv = crate::test_helpers::server();
		let mut k = Kern::new("kx", "");
		for e in entities {
			k.entities.insert(e.id.clone(), e);
		}
		srv.graph.write().kerns.insert("kx".into(), k);
		srv
	}

	fn events(v: &serde_json::Value) -> Vec<serde_json::Value> {
		v["events"].as_array().cloned().unwrap_or_default()
	}

	#[test]
	fn cursor_encode_decode_roundtrips() {
		let c = EventCursor {
			nanos: 123456789,
			entity_id: "deadbeef".into(),
			change_ord: 1,
		};
		let back = EventCursor::decode(&c.encode()).expect("roundtrip");
		assert!(back == c, "cursor must survive the wire round trip");
		assert!(EventCursor::decode("not-a-cursor").is_none());
	}

	#[tokio::test]
	async fn from_zero_returns_both_creates_then_a_replay_is_empty() {
		let srv = server_with(vec![ent("e1", 100), ent("e2", 200)]);

		let out = srv.tool_events(&serde_json::json!({"since": 0}));
		assert!(out.is_ok(), "{out:?}");
		let v = out.unwrap();
		let evs = events(&v);
		assert_eq!(evs.len(), 2, "both ingests surface as events: {v}");
		for e in &evs {
			assert_eq!(e["change"], "created", "a fresh ingest is a `created`: {e}");
			assert_eq!(e["kind"], "claim");
			assert_eq!(e["source_scheme"], "inline");
			assert!(
				e["at"].as_str().is_some(),
				"each event carries its own cursor"
			);
		}
		let cursor = v["cursor"]
			.as_str()
			.expect("a cursor to resume from")
			.to_string();
		assert!(
			!cursor.is_empty() && cursor != "0",
			"a non-empty cursor: {cursor}"
		);

		// Second poll from that cursor: nothing new, and the cursor is not rewound.
		let out = srv.tool_events(&serde_json::json!({"since": cursor}));
		let v = out.unwrap();
		assert!(
			events(&v).is_empty(),
			"a caught-up poll returns no events: {v}"
		);
		assert_eq!(
			v["cursor"].as_str().unwrap(),
			cursor,
			"a quiet poll echoes the position back"
		);
	}

	#[tokio::test]
	async fn a_supersede_surfaces_as_superseded_plus_the_new_created() {
		// Poll once so we have the cursor that predates the supersede.
		let srv = server_with(vec![ent("e1", 100)]);
		let out = srv.tool_events(&serde_json::json!({"since": 0}));
		let prior = out.unwrap()["cursor"].as_str().unwrap().to_string();

		// Supersede e1 (invalidated at 300) and ingest the new revision e2 (created
		// at 200) — the shape base/accept.rs leaves after a re-ingest.
		{
			let mut g = srv.graph.write();
			let k = g.kerns.get_mut("kx").unwrap();
			let old = k.entities.remove("e1").unwrap();
			k.entities.insert("e1".into(), superseded(old, 300));
			k.entities.insert("e2".into(), ent("e2", 200));
		}

		let out = srv.tool_events(&serde_json::json!({"since": prior}));
		let v = out.unwrap();
		let evs = events(&v);
		let kinds: Vec<(&str, &str)> = evs
			.iter()
			.map(|e| {
				(
					e["entity_id"].as_str().unwrap(),
					e["change"].as_str().unwrap(),
				)
			})
			.collect();
		assert!(
			kinds.contains(&("e2", "created")),
			"the new revision is a created event: {kinds:?}"
		);
		assert!(
			kinds.contains(&("e1", "superseded")),
			"the invalidated revision is a superseded event: {kinds:?}"
		);
		assert!(
			!kinds.contains(&("e1", "created")),
			"e1's original created was already delivered — no overlap: {kinds:?}"
		);
	}

	#[tokio::test]
	async fn limit_bounds_the_batch_and_the_cursor_resumes_without_gap_or_overlap() {
		let srv = server_with(vec![ent("e1", 100), ent("e2", 200), ent("e3", 300)]);

		let out = srv.tool_events(&serde_json::json!({"since": 0, "limit": 2}));
		let v = out.unwrap();
		let first = events(&v);
		assert_eq!(first.len(), 2, "limit caps the batch at two: {v}");
		let cursor = v["cursor"].as_str().unwrap().to_string();

		let out = srv.tool_events(&serde_json::json!({"since": cursor, "limit": 2}));
		let v = out.unwrap();
		let second = events(&v);
		assert_eq!(
			second.len(),
			1,
			"the remaining event resumes on the next poll: {v}"
		);

		// No gap, no overlap: the three ids appear exactly once across both batches.
		let mut seen: Vec<String> = first
			.iter()
			.chain(second.iter())
			.map(|e| e["entity_id"].as_str().unwrap().to_string())
			.collect();
		seen.sort();
		seen.dedup();
		assert_eq!(
			seen,
			vec!["e1", "e2", "e3"],
			"every id delivered once: {seen:?}"
		);
	}

	#[tokio::test]
	async fn a_malformed_string_cursor_is_rejected() {
		let srv = server_with(vec![ent("e1", 100)]);
		let out = srv.tool_events(&serde_json::json!({"since": "garbage"}));
		assert!(
			out.is_err(),
			"a cursor that cannot be decoded is an error, not a silent rewind"
		);
		let msg = out.unwrap_err();
		assert!(msg.contains("since"), "the error names the field: {msg}");
	}
}
