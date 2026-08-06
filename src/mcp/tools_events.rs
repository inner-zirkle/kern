use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::{tool_error, tool_result_json, Server};

// A default cap so a first poll of a large graph does not return every event
// ever recorded in one payload; the returned `cursor` resumes the rest. A
// Watcher that wants a bigger batch sets `limit` explicitly.
const DEFAULT_LIMIT: usize = 100;

pub(crate) fn tool_schemas() -> Vec<serde_json::Value> {
	vec![serde_json::json!({
		"name": "events",
		"description": "Read-only change feed for polling: what changed in memory since an opaque cursor. Derives events from the state kern already keeps — bitemporal timestamps on entities — without mutating the graph. Returns `created` for each entity ingested after the cursor and `superseded` for each revision invalidated after it, ordered by the cursor ascending. Pass `since` = the `cursor` a prior call returned (0 or absent = from the beginning) and the next call resumes without gap or overlap; `limit` bounds the batch. This is the Event source a ctrl Watcher polls (see actions.md). `degraded`/`forgotten` are declared for the wire contract but not emitted: a forgotten entity leaves no resident row and `degrade` touches edges, not entity timestamps — neither is derivable read-only.",
		"inputSchema": {
			"type": "object",
			"properties": {
				"since": {"description": "opaque cursor a prior call returned; 0 or absent = from the beginning"},
				"limit": {"type": "integer", "description": "max events in this batch (default 100)"},
			},
		},
	})]
}

// created before superseded when two events on one entity ever share a nanosecond
// stamp (they cannot in practice — an entity is invalidated strictly after it is
// created — but the ordinal pins the tie-break so the cursor is a total order).
const CHANGE_CREATED: (&str, u8) = ("created", 0);
const CHANGE_SUPERSEDED: (&str, u8) = ("superseded", 1);

// The total order the feed is sorted by and the cursor resumes on. Field
// declaration order IS the comparison order (derived Ord): nanos, then entity
// id, then the change ordinal. Encoded to an opaque `nanos.ord.entity_id`
// string on the wire; compared as this tuple, never lexically, so the width of
// the numeric part is irrelevant to correctness.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct EventCursor {
	nanos: u128,
	entity_id: String,
	change_ord: u8,
}

impl EventCursor {
	fn encode(&self) -> String {
		format!("{}.{}.{}", self.nanos, self.change_ord, self.entity_id)
	}

	// `nanos.ord.entity_id`. The entity id is a content hash (no '.'), so a
	// 3-way split is unambiguous. Returns None on any malformed cursor so a
	// caller learns its cursor was rejected rather than silently rewinding.
	fn decode(s: &str) -> Option<Self> {
		let mut it = s.splitn(3, '.');
		let nanos: u128 = it.next()?.parse().ok()?;
		let change_ord: u8 = it.next()?.parse().ok()?;
		let entity_id = it.next()?.to_string();
		Some(EventCursor {
			nanos,
			entity_id,
			change_ord,
		})
	}
}

fn nanos_of(t: SystemTime) -> u128 {
	t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

// A single change, gathered under the read guard with its label projection so
// the render pass is a pure map over the sorted list — no second graph walk.
struct PendingEvent {
	cursor: EventCursor,
	change: &'static str,
	kind: &'static str,
	scheme: &'static str,
}

#[derive(Deserialize, Default)]
struct EventsArgs {
	// Untyped: a first call passes the integer 0, later calls pass back the
	// opaque string cursor. Both are accepted here and normalized below.
	#[serde(default)]
	since: serde_json::Value,
	#[serde(default)]
	limit: Option<usize>,
}

// The lower bound the feed resumes strictly after. `None` = from the beginning.
// A non-zero integer is treated as a bare nanosecond lower bound (ord 0, empty
// id), so `since: <nanos>` still means "after this instant".
fn parse_since(v: &serde_json::Value) -> Result<Option<EventCursor>, String> {
	match v {
		serde_json::Value::Null => Ok(None),
		serde_json::Value::Number(n) => {
			let raw = n.as_u64().ok_or_else(|| format!("invalid `since`: {n}"))?;
			if raw == 0 {
				Ok(None)
			} else {
				Ok(Some(EventCursor {
					nanos: raw as u128,
					entity_id: String::new(),
					change_ord: 0,
				}))
			}
		}
		serde_json::Value::String(s) => {
			if s.is_empty() || s == "0" {
				return Ok(None);
			}
			EventCursor::decode(s)
				.map(Some)
				.ok_or_else(|| format!("invalid `since` cursor: {s}"))
		}
		other => Err(format!("invalid `since`: {other}")),
	}
}

impl Server {
	pub(crate) fn tool_events(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: EventsArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};
		let since = match parse_since(&p.since) {
			Ok(s) => s,
			Err(e) => return tool_error(&e),
		};
		let limit = match p.limit {
			Some(0) | None => DEFAULT_LIMIT,
			Some(n) => n,
		};

		// One read guard, no mutation: walk the resident kerns exactly as the
		// health/adjacency passes do and read the bitemporal stamps kern already
		// keeps. `created_at` dates the ingest; `invalidated_at` (set only on a
		// supersede, with status flipped) dates the revision going stale. Each
		// event carries its label projection so rendering needs no second walk.
		let mut events: Vec<PendingEvent> = Vec::new();
		{
			let g = self.graph.read();
			for kern in g.all() {
				for e in kern.entities.values() {
					let scheme = e.source.scheme();
					let kind = e.kind.as_str();
					if let Some(created) = e.created_at {
						events.push(PendingEvent {
							cursor: EventCursor {
								nanos: nanos_of(created),
								entity_id: e.id.clone(),
								change_ord: CHANGE_CREATED.1,
							},
							change: CHANGE_CREATED.0,
							kind,
							scheme,
						});
					}
					if e.is_superseded() {
						if let Some(inv) = e.invalidated_at {
							events.push(PendingEvent {
								cursor: EventCursor {
									nanos: nanos_of(inv),
									entity_id: e.id.clone(),
									change_ord: CHANGE_SUPERSEDED.1,
								},
								change: CHANGE_SUPERSEDED.0,
								kind,
								scheme,
							});
						}
					}
				}
			}
		}

		// Order by the cursor ascending, drop everything at or before `since`
		// (strictly greater = no overlap with the last delivered event), then cap.
		events.sort_by(|a, b| a.cursor.cmp(&b.cursor));
		let mut out: Vec<serde_json::Value> = Vec::new();
		let mut last: Option<EventCursor> = None;
		for ev in events {
			if let Some(ref lo) = since {
				if ev.cursor <= *lo {
					continue;
				}
			}
			if out.len() >= limit {
				break;
			}
			out.push(serde_json::json!({
				"entity_id": ev.cursor.entity_id,
				"kind": ev.kind,
				"change": ev.change,
				"at": ev.cursor.encode(),
				"source_scheme": ev.scheme,
			}));
			last = Some(ev.cursor);
		}

		// The next cursor: the last event we emitted, or — when nothing changed —
		// the caller's own position echoed back so a quiet poll never rewinds it.
		let cursor = last
			.map(|c| c.encode())
			.or_else(|| since.as_ref().map(EventCursor::encode))
			.unwrap_or_else(|| "0".to_string());

		tool_result_json(&serde_json::json!({
			"events": out,
			"cursor": cursor,
		}))
	}
}

#[cfg(test)]
mod tests {
	use super::EventCursor;
	use crate::base::types::{Entity, EntityKind, EntityStatus, Kern, Source};
	use crate::mcp::Server;
	use crate::test_support::tool_text as text;
	use crate::mcp::tools::is_error;
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
		let srv = crate::test_support::mcp_server();
		let mut k = Kern::new("kx", "");
		for e in entities {
			k.entities.insert(e.id.clone(), e);
		}
		srv.graph.write().kerns.insert("kx".into(), k);
		srv
	}

	fn body(out: &serde_json::Value) -> serde_json::Value {
		serde_json::from_str(&text(out)).expect("success body is json")
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
		assert!(!is_error(&out), "{}", text(&out));
		let v = body(&out);
		let evs = events(&v);
		assert_eq!(evs.len(), 2, "both ingests surface as events: {v}");
		for e in &evs {
			assert_eq!(e["change"], "created", "a fresh ingest is a `created`: {e}");
			assert_eq!(e["kind"], "claim");
			assert_eq!(e["source_scheme"], "inline");
			assert!(e["at"].as_str().is_some(), "each event carries its own cursor");
		}
		let cursor = v["cursor"].as_str().expect("a cursor to resume from").to_string();
		assert!(!cursor.is_empty() && cursor != "0", "a non-empty cursor: {cursor}");

		// Second poll from that cursor: nothing new, and the cursor is not rewound.
		let out = srv.tool_events(&serde_json::json!({"since": cursor}));
		let v = body(&out);
		assert!(events(&v).is_empty(), "a caught-up poll returns no events: {v}");
		assert_eq!(v["cursor"].as_str().unwrap(), cursor, "a quiet poll echoes the position back");
	}

	#[tokio::test]
	async fn a_supersede_surfaces_as_superseded_plus_the_new_created() {
		// Poll once so we have the cursor that predates the supersede.
		let srv = server_with(vec![ent("e1", 100)]);
		let out = srv.tool_events(&serde_json::json!({"since": 0}));
		let prior = body(&out)["cursor"].as_str().unwrap().to_string();

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
		let v = body(&out);
		let evs = events(&v);
		let kinds: Vec<(&str, &str)> = evs
			.iter()
			.map(|e| (e["entity_id"].as_str().unwrap(), e["change"].as_str().unwrap()))
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
		let v = body(&out);
		let first = events(&v);
		assert_eq!(first.len(), 2, "limit caps the batch at two: {v}");
		let cursor = v["cursor"].as_str().unwrap().to_string();

		let out = srv.tool_events(&serde_json::json!({"since": cursor, "limit": 2}));
		let v = body(&out);
		let second = events(&v);
		assert_eq!(second.len(), 1, "the remaining event resumes on the next poll: {v}");

		// No gap, no overlap: the three ids appear exactly once across both batches.
		let mut seen: Vec<String> = first
			.iter()
			.chain(second.iter())
			.map(|e| e["entity_id"].as_str().unwrap().to_string())
			.collect();
		seen.sort();
		seen.dedup();
		assert_eq!(seen, vec!["e1", "e2", "e3"], "every id delivered once: {seen:?}");
	}

	#[tokio::test]
	async fn a_malformed_string_cursor_is_rejected() {
		let srv = server_with(vec![ent("e1", 100)]);
		let out = srv.tool_events(&serde_json::json!({"since": "garbage"}));
		assert!(is_error(&out), "a cursor that cannot be decoded is an error, not a silent rewind");
		assert!(text(&out).contains("since"), "the error names the field: {}", text(&out));
	}
}
