use serde::Deserialize;

use crate::search::find_entity_by_prefix;
use crate::base_types::EntityKind;
use crate::util::truncate;

use crate::retrieval;

pub(crate) fn tool_schemas() -> Vec<serde_json::Value> {
	vec![serde_json::json!({
		"name": "query",
		"description": "Search the knowledge graph. Returns scored thoughts with edges and path chains — no synthesis: the calling agent reads the passages and synthesizes. Requires at least one of `text` (semantic/lexical search), `id` (direct lookup) or `ids` (batch direct lookup).",
		"inputSchema": {
			"type": "object",
			// No top-level anyOf/oneOf/allOf: the Anthropic tool API rejects a
			// combinator at the root of input_schema. The "at least one of text,
			// id, ids" rule lives in the description and in tool_query's runtime
			// guard.
			"properties": {
				"text":      {"type": "string", "description": "search query text"},
				"id":        {"type": "string", "description": "thought ID for direct lookup"},
				"ids":       {"type": "array", "items": {"type": "string"}, "description": "batch direct lookup — returns {results:[...], missing:[...]}; each id resolves a prefix and the cold tier and honours the same filters as `id`"},
				"k":         {"type": "integer", "description": "number of results (default 5)"},
				"mode":      {"type": "string", "enum": ["content", "reason", "hybrid"], "description": "retrieval mode (default hybrid)"},
				"sort":      {"type": "string", "enum": ["", "date", "access", "confidence"], "description": "sort key"},
				"ascending": {"type": "boolean", "description": "sort ascending (default false)"},
				"source":    {"type": "string", "description": "filter by source system"},
				"kind":      {"type": "string", "enum": ["", "fact", "claim", "document", "question", "conclusion"], "description": "filter by thought kind"},
				"claim_kind": {"type": "string", "description": "filter distilled claims by claim-kind label (builtin or registered). Sub-kind hierarchy is honoured: filtering on a parent kind also returns claims labelled with any registered sub-kind (subClassOf closure)"},
				"since":     {"type": "string", "description": "ISO8601 timestamp; only include thoughts at or after this time"},
				"before":    {"type": "string", "description": "ISO8601 timestamp; only include thoughts before this time"},
				"min_conf":  {"type": "number", "description": "minimum confidence 0.0-1.0"},
				"as_of":     {"type": "string", "description": "ISO8601 timestamp; bi-temporal point query — return only the revision whose validity window [valid_from, valid_to) covered this instant"},
				"valid_at":  {"type": "string", "description": "ISO8601 timestamp; only include thoughts whose valid_until (TTL) has not passed at this instant"},
				"scheme":    {"type": "string", "enum": ["file", "ticket", "session", "agent", "inline"], "description": "filter by source scheme"},
				"include_history": {"type": "boolean", "description": "also return superseded (invalidated) revisions reachable from the active hits, flagged history:true"},
				"exclude_pending": {"type": "boolean", "description": "drop thoughts a review policy is still holding for curation (`review: pending`, set by `[ingest] review_policy`) — the `promote` tool releases one. Opt-in: omitting it filters nothing, so an uncurated graph reads exactly as before"},
			},
		},
	})]
}

use crate::mcp::{tool_error, tool_result_json, Server};

fn parse_time_filter(field: &str, value: &str) -> Result<Option<std::time::SystemTime>, String> {
	if value.is_empty() {
		return Ok(None);
	}
	crate::time::parse_rfc3339(value)
		.map(Some)
		.map_err(|()| format!("invalid `{field}` timestamp: {value}"))
}

fn build_query_options(p: &QueryArgs) -> Result<retrieval::score::QueryOptions, String> {
	let mut opts = retrieval::score::QueryOptions {
		sort: retrieval::score::SortField::parse(&p.sort),
		ascending: p.ascending,
		source: p.source.clone(),
		kind: p.kind,
		min_conf: p.min_conf,
		since: parse_time_filter("since", &p.since)?,
		before: parse_time_filter("before", &p.before)?,
		valid_at: parse_time_filter("valid_at", &p.valid_at)?,
		as_of: parse_time_filter("as_of", &p.as_of)?,
		include_history: p.include_history,
		exclude_pending: p.exclude_pending,
		..Default::default()
	};
	if let Some(ref s) = p.scheme {
		match crate::base_types::Source::parse_scheme(s) {
			Some(tag) => opts.scheme = Some(tag.to_string()),
			None => return Err(format!("unknown source scheme: {s}")),
		}
	}
	Ok(opts)
}

#[derive(Deserialize, Default)]
struct QueryArgs {
	#[serde(default)]
	text: String,
	#[serde(default)]
	id: String,
	#[serde(default)]
	ids: Vec<String>,
	#[serde(default)]
	k: usize,
	#[serde(default)]
	mode: String,
	#[serde(default)]
	sort: String,
	#[serde(default)]
	ascending: bool,
	#[serde(default)]
	source: String,
	#[serde(default, deserialize_with = "de_kind")]
	kind: Option<EntityKind>,
	#[serde(default)]
	claim_kind: String,
	#[serde(default)]
	scheme: Option<String>,
	#[serde(default)]
	since: String,
	#[serde(default)]
	before: String,
	#[serde(default)]
	min_conf: f64,
	#[serde(default)]
	valid_at: String,
	#[serde(default)]
	as_of: String,
	#[serde(default)]
	include_history: bool,
	#[serde(default)]
	exclude_pending: bool,
}

// The filter takes the stable lowercase labels (`EntityKind::as_str`), not the
// Rust variant names serde derive would expect.
fn de_kind<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<EntityKind>, D::Error> {
	let s = Option::<String>::deserialize(d)?;
	match s.as_deref() {
		None | Some("") => Ok(None),
		Some(v) => EntityKind::parse(v)
			.map(Some)
			.ok_or_else(|| serde::de::Error::custom(format!("unknown kind: {v}"))),
	}
}

impl Server {
	// Resolves a `claim_kind` label to its subClassOf closure against the live
	// registry, so `matches_filter` stays graph-free. Unknown labels are an
	// error, not an empty result — a typo should say so.
	fn apply_claim_kind_filter(
		&self,
		p: &QueryArgs,
		opts: &mut retrieval::score::QueryOptions,
	) -> Result<(), String> {
		if p.claim_kind.is_empty() {
			return Ok(());
		}
		let g = self.graph.read();
		let known = crate::ingest::distill::DEFAULT_KINDS.contains(&p.claim_kind.as_str())
			|| g.root.claim_kinds.contains_key(&p.claim_kind);
		if !known {
			return Err(format!("unknown claim kind: {}", p.claim_kind));
		}
		opts.claim_kinds = Some(g.root.claim_kind_closure(&p.claim_kind));
		Ok(())
	}

	#[allow(clippy::field_reassign_with_default)]
	pub(crate) fn tool_query(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: QueryArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};

		if !p.ids.is_empty() {
			// Batch direct lookup: same filters as `id`, applied per row. Returns
			// `{results, missing}` so a caller can tell a filter-drop (in results,
			// flagged) from a non-existent id (in missing). Prefix and cold tier
			// both resolve, matching the single-id path.
			let mut opts = match build_query_options(&p) {
				Ok(o) => o,
				Err(e) => return tool_error(&e),
			};
			if let Err(e) = self.apply_claim_kind_filter(&p, &mut opts) {
				return tool_error(&e);
			}
			let g = self.graph.read();
			let mut results = Vec::new();
			let mut missing = Vec::new();
			for id in &p.ids {
				match resolve_by_id(&g, id)
					.filter(|hit| retrieval::score::matches_filter(&hit.thought, &opts))
				{
					Some(hit) => results.push(hit.detail(&g)),
					None => missing.push(id.clone()),
				}
			}
			return tool_result_json(&serde_json::json!({"results": results, "missing": missing}));
		}

		if !p.id.is_empty() {
			// The same filters the ranked read honours, applied to the one row an
			// id names: `query {id, kind: "claim"}` that answered with a Fact would
			// make the filter mean one thing on `text` and nothing on `id`.
			// A bare `query {id}` still serves everything — `QueryOptions::default()`
			// leaves every filter off, `valid_at`/`as_of` included, so an expired row
			// keeps arriving flagged rather than filtered.
			let mut opts = match build_query_options(&p) {
				Ok(o) => o,
				Err(e) => return tool_error(&e),
			};
			if let Err(e) = self.apply_claim_kind_filter(&p, &mut opts) {
				return tool_error(&e);
			}
			let g = self.graph.read();
			// Prefix and cold tier both included so `kern get` can route here
			// without resolving fewer ids than it did reading the store itself.
			let hit = resolve_by_id(&g, &p.id)
				.filter(|hit| retrieval::score::matches_filter(&hit.thought, &opts));
			return match hit {
				Some(hit) => tool_result_json(&hit.detail(&g)),
				None => tool_error(&format!("thought not found: {}", p.id)),
			};
		}

		if p.text.is_empty() {
			return tool_error("either text, id or ids is required");
		}

		let llm = match &self.llm {
			Some(c) => c.clone(),
			None => return tool_error("no embed client configured"),
		};

		let mode = retrieval::seed::Mode::parse(&p.mode);
		let rcfg = &self.cfg.retrieval;

		let vec = match crate::llm::block_on_in_place(llm.embed(&p.text)) {
			Some(Ok(v)) => v,
			Some(Err(e)) => return tool_error(&format!("embed failed: {e}")),
			None => return tool_error("no tokio runtime"),
		};

		let mut opts = match build_query_options(&p) {
			Ok(o) => o,
			Err(e) => return tool_error(&e),
		};
		if let Err(e) = self.apply_claim_kind_filter(&p, &mut opts) {
			return tool_error(&e);
		}
		let opts = opts;

		let result = retrieval::query::query_locked(
			&self.graph,
			rcfg,
			&self.cfg.heat,
			&vec,
			&p.text,
			mode,
			Some(opts.clone()),
		);
		// query_locked took only a read lock; access stamps commit off the hot
		// path via CommitAccess (advisory, skipped without a queue).
		if let Some(ref q) = self.task_q {
			let ids: Vec<String> = result
				.entities
				.iter()
				.map(|s| s.entity.id.clone())
				.collect();
			if !ids.is_empty() {
				q.enqueue(crate::tick_queue::task_commit_access(&ids));
			}
		}
		let vec = Some(vec);
		(self.save_fn)();

		let k = if p.k == 0 { rcfg.seed_k } else { p.k };

		let mut scored: Vec<retrieval::expand::ScoredEntity> = result.entities.clone();
		let mut cold_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
		// Exact-text fast path skipped embedding (`vec` None), so cold-tier fill is skipped too.
		if let Some(ref vec) = vec {
			if scored.len() < k {
				// Clone the store handle under a brief read guard; drop it before the scan.
				let store = self.graph.read().store();
				let have: std::collections::HashSet<String> =
					scored.iter().map(|s| s.entity.id.clone()).collect();
				if let Some(store) = &store {
					for (entity, score) in store.cold_search(vec, k).unwrap_or_default() {
						if scored.len() >= k {
							break;
						}
						// cold_search is a raw cosine scan of the spill tier — it answers no
						// filter. Delivering its hits unfiltered made the cold tier a way
						// around every predicate the hot path enforces.
						if !retrieval::score::matches_filter(&entity, &opts) {
							continue;
						}
						if !have.contains(&entity.id) {
							cold_ids.insert(entity.id.clone());
							scored.push(retrieval::expand::ScoredEntity { entity, score });
						}
					}
				}
			}
		}

		// The ANN never holds Superseded rows; walk Supersedes chains back from the
		// active hits for history.
		let mut history_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
		if p.include_history {
			// The same `opts` the ranked read and the cold fill used — rebuilding it
			// here was a second chance for the three paths to disagree about what the
			// caller asked for.
			let g = self.graph.read();
			let heads: Vec<(String, f64)> = scored
				.iter()
				.map(|s| (s.entity.id.clone(), s.score))
				.collect();
			let mut have: std::collections::HashSet<String> =
				scored.iter().map(|s| s.entity.id.clone()).collect();
			for (head_id, head_score) in heads {
				for anc_id in crate::reason::superseded_ancestors(&g, &head_id) {
					if !have.insert(anc_id.clone()) {
						continue;
					}
					let ancestor = g
						.kern_of_entity(&anc_id)
						.and_then(|kid| g.kerns.get(kid))
						.and_then(|k| k.entities.get(&anc_id))
						.cloned()
						.or_else(|| g.store().and_then(|s| s.cold_get(&anc_id).ok().flatten()));
					if let Some(ent) = ancestor {
						if retrieval::score::matches_filter(&ent, &opts) {
							history_ids.insert(anc_id.clone());
							scored.push(retrieval::expand::ScoredEntity {
								entity: ent,
								score: head_score,
							});
						}
					}
				}
			}
		}

		let take_n = k + history_ids.len();
		let entities: Vec<serde_json::Value> = {
			let g = self.graph.read();
			scored
				.iter()
				.take(take_n)
				.map(|st| {
					let edges: Vec<serde_json::Value> = g
						.kern_of_entity(&st.entity.id)
						.and_then(|kid| g.kerns.get(kid))
						.map(|kern| {
							crate::reason::collect_reason_ids(kern, &st.entity.id)
								.into_iter()
								.filter_map(|rid| kern.reasons.get(&rid))
								.filter(|r| r.is_enriched())
								.map(|r| {
									serde_json::json!({
										"from": r.from,
										"to": r.to,
										"kind": r.kind as i32,
										"text": truncate(&r.text, 120),
										"score": r.score,
									})
								})
								.collect()
						})
						.unwrap_or_default();
					let mut v = base_entity_json(&st.entity, st.score);
					v["cold"] = serde_json::Value::Bool(cold_ids.contains(&st.entity.id));
					if history_ids.contains(&st.entity.id) {
						v["history"] = serde_json::Value::Bool(true);
					}
					if !edges.is_empty() {
						v["edges"] = serde_json::Value::Array(edges);
					}
					v
				})
				.collect()
		};

		let chains = {
			let g = self.graph.read();
			retrieval::query::format_chains(&g, &result.path_chains)
		};

		tool_result_json(&serde_json::json!({"entities": entities, "chains": chains}))
	}
}

// What the CLI prints for a thought no kern still holds. A cold hit has no kern
// id, and the label is the one `kern get` has always shown for that case.
const COLD_KERN: &str = "(cold)";

// The one id resolver behind both the `query` tool and `kern get`: a second one
// would let the routed and local reads disagree about what an id resolves to —
// prefix or cold, resolved here or resolved by a daemon, same answer.
pub(crate) fn entity_detail_by_id(
	g: &crate::graph::GraphGnn,
	id: &str,
) -> Option<serde_json::Value> {
	let hit = resolve_by_id(g, id)?;
	Some(hit.detail(g))
}

// A resolved id read, before it is rendered. Resolving and rendering are split
// so the `query` tool can put the row through `matches_filter` — the same
// predicate the ranked read uses — while still resolving ids exactly one way.
struct IdHit {
	thought: crate::base_types::Entity,
	kern_id: String,
	cold: bool,
}

impl IdHit {
	fn detail(&self, g: &crate::graph::GraphGnn) -> serde_json::Value {
		let mut v = entity_detail(&self.thought, &self.kern_id, g);
		if self.cold {
			// The label is for the printer; the flag is for anything reading the
			// JSON, which should not have to match on a sentinel kern id.
			v["cold"] = serde_json::Value::Bool(true);
		}
		v
	}
}

fn resolve_by_id(g: &crate::graph::GraphGnn, id: &str) -> Option<IdHit> {
	if let Some((thought, kern_id)) = find_entity_by_prefix(g, id) {
		return Some(IdHit {
			thought,
			kern_id,
			cold: false,
		});
	}
	let thought = g.store().and_then(|s| s.cold_get(id).ok().flatten())?;
	Some(IdHit {
		thought,
		kern_id: COLD_KERN.to_string(),
		cold: true,
	})
}

fn entity_detail(
	thought: &crate::base_types::Entity,
	kern_id: &str,
	g: &crate::graph::GraphGnn,
) -> serde_json::Value {
	let mut edges = Vec::new();
	if let Some(kern) = g.kerns.get(kern_id) {
		let rids = crate::reason::collect_reason_ids(kern, &thought.id);
		for rid in &rids {
			if let Some(re) = kern.reasons.get(rid) {
				edges.push(serde_json::json!({
					"id": re.id,
					"from": re.from,
					"to": re.to,
					"kind": re.kind as i32,
					"text": re.text.clone(),
					"score": re.score,
				}));
			}
		}
	}
	let mut v = serde_json::json!({
		"id": thought.id,
		"kind": thought.kind as u8,
		"text": thought.text(),
		"score": thought.score,
		"conf": thought.conf_mean(),
		"conf_uncertainty": thought.conf_variance(),
		"access_count": thought.access_count.value_i32(),
		"kern": kern_id,
		"source": {
			"scheme": thought.source.scheme(),
			"object_id": thought.source.object_id(),
			"section": thought.source.section(),
			"url": thought.source.url(),
		},
		"edges": edges,
	});
	// Retention on the id surface. The ranked path DROPS an expired thought
	// (`score::drop_expired`); an explicit id names one row, so answering "thought
	// not found" for a row that is demonstrably on disk — and that GC never
	// collects, since a non-superseded Fact is GC-immune — would be a lie the
	// caller cannot falsify. It is annotated instead, the way a cold hit is:
	// served, flagged, deadline included, caller decides.
	if let Some(exp) = thought.valid_until {
		v["valid_until"] = serde_json::json!(secs_since_epoch(exp));
		v["expired"] = serde_json::Value::Bool(exp < std::time::SystemTime::now());
	}
	v
}

fn secs_since_epoch(t: std::time::SystemTime) -> u64 {
	t.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

// kind/scheme/status labels are consumed by `kern_rpc::query` — do not drop them.
// The full `source` backlink (object_id/section/url) rides alongside them, the exact
// shape the id-lookup path emits, so a ranked hit can be followed back to the proving
// corpus page rather than surfacing only the scheme it matched on. It is placed first
// in the envelope on purpose: the wire contract is "ranked recall carries the backlink".
pub(crate) fn base_entity_json(
	entity: &crate::base_types::Entity,
	score: f64,
) -> serde_json::Value {
	let status_str = if entity.is_superseded() { "superseded" } else { "active" };
	serde_json::json!({
		"id": entity.id,
		"source": {
			"scheme": entity.source.scheme(),
			"object_id": entity.source.object_id(),
			"section": entity.source.section(),
			"url": entity.source.url(),
		},
		"score": score,
		"conf": entity.conf_mean(),
		"conf_uncertainty": entity.conf_variance(),
		"text": truncate(&entity.text(), 500),
		"kind": entity.kind.as_str(),
		"scheme": entity.source.scheme(),
		"status": status_str,
	})
}

#[cfg(test)]
mod envelope_shape_tests {
	use super::base_entity_json as build_entity_json;
	use crate::base_types::{ChunkPart, ChunkPartKind, Entity, EntityKind, EntityStatus, Source};

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
		assert_eq!(source.get("scheme").and_then(|x| x.as_str()), Some("inline"));
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

#[cfg(test)]
mod id_filter_tests {
	use crate::base_types::{Entity, EntityKind, Kern, Source};
	use crate::mcp::Server;
	use crate::test_support::tool_text as text;
	use crate::mcp::tools::is_error;

	fn server_with(thought: Entity) -> Server {
		let srv = crate::test_support::mcp_server();
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

	fn body(out: &serde_json::Value) -> serde_json::Value {
		serde_json::from_str(&text(out)).expect("success body is json")
	}

	#[tokio::test]
	async fn id_read_drops_a_row_the_kind_filter_excludes() {
		let srv = server_with(fact("f1"));
		let out = srv.tool_query(&serde_json::json!({"id": "f1", "kind": "claim"}));
		assert!(
			is_error(&out),
			"a Fact must not survive kind=claim just because it was named by id: {out}"
		);
		assert!(text(&out).contains("thought not found"));
	}

	#[tokio::test]
	async fn id_read_keeps_a_row_the_filters_admit() {
		let srv = server_with(fact("f1"));
		let out = srv.tool_query(&serde_json::json!({
			"id": "f1", "kind": "fact", "scheme": "inline",
		}));
		assert!(!is_error(&out), "matching filters must not hide it: {out}");
		assert_eq!(body(&out)["id"], serde_json::json!("f1"));
	}

	#[tokio::test]
	async fn id_read_reports_a_bad_filter_rather_than_ignoring_it() {
		let srv = server_with(fact("f1"));
		let out = srv.tool_query(&serde_json::json!({"id": "f1", "since": "not-a-time"}));
		assert!(is_error(&out));
		assert!(
			text(&out).contains("since"),
			"names the field: {}",
			text(&out)
		);
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
		srv.graph
			.write()
			.root
			.add_claim_kind(
				"rust-fact",
				"rust facts",
				Some("code-fact"),
				&crate::ingest::distill::DEFAULT_KINDS,
			)
			.expect("builtin parent registers");

		let out = srv.tool_query(&serde_json::json!({"id": "c1", "claim_kind": "code-fact"}));
		assert!(
			!is_error(&out),
			"a rust-fact claim answers a code-fact filter through the hierarchy: {out}"
		);
		assert_eq!(body(&out)["id"], serde_json::json!("c1"));

		let out = srv.tool_query(&serde_json::json!({"id": "c1", "claim_kind": "preference"}));
		assert!(
			is_error(&out),
			"an unrelated claim kind must not match: {out}"
		);
	}

	#[tokio::test]
	async fn claim_kind_filter_rejects_an_unknown_label_instead_of_matching_nothing() {
		let srv = server_with(distilled_claim("c1", "decision"));
		let out = srv.tool_query(&serde_json::json!({"id": "c1", "claim_kind": "ghost"}));
		assert!(is_error(&out));
		assert!(
			text(&out).contains("unknown claim kind"),
			"a typo says so: {}",
			text(&out)
		);
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
		let srv = crate::test_support::mcp_server();
		srv.graph.write().kerns.insert("kx".into(), k);

		let out = srv.tool_query(&serde_json::json!({"ids": ["f1", "f2", "ghost"]}));
		assert!(!is_error(&out), "batch is not an error: {out}");
		let v = body(&out);
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
		let srv = crate::test_support::mcp_server();
		srv.graph.write().kerns.insert("kx".into(), k);

		let out = srv.tool_query(&serde_json::json!({"ids": ["f1", "c1"], "kind": "fact"}));
		let v = body(&out);
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
		let deadline = crate::time::parse_rfc3339("2020-01-01T00:00:00Z").expect("fixed ts");
		e.valid_until = Some(deadline);
		let srv = server_with(e);

		let out = srv.tool_query(&serde_json::json!({"id": "f1"}));
		assert!(
			!is_error(&out),
			"'not found' would lie about a row that is demonstrably on disk: {out}"
		);
		let v = body(&out);
		assert_eq!(v["expired"], serde_json::json!(true));
		assert!(
			v.get("valid_until").is_some(),
			"deadline travels with the flag"
		);

		// Ask for validity explicitly and it is a filter again, like any other.
		let out = srv.tool_query(&serde_json::json!({
			"id": "f1", "valid_at": "2026-01-01T00:00:00Z",
		}));
		assert!(is_error(&out), "an explicit valid_at does filter: {out}");
	}

	// Item 21's hold half, through the surface rather than the engine. Both
	// directions, for the reason `exclude_pending_drops_only_the_uncurated_and_
	// only_when_asked` gives: a held row that is never in the set proves nothing.
	#[tokio::test]
	async fn id_read_withholds_a_held_row_only_when_exclude_pending_is_asked() {
		let mut e = fact("f1");
		e.review = crate::base_types::ReviewState::Pending;
		let srv = server_with(e);

		let out = srv.tool_query(&serde_json::json!({"id": "f1"}));
		assert!(
			!is_error(&out),
			"opt-in: nobody asked, so the held row still arrives: {out}"
		);
		assert_eq!(body(&out)["id"], serde_json::json!("f1"));

		let out = srv.tool_query(&serde_json::json!({"id": "f1", "exclude_pending": true}));
		assert!(
			is_error(&out),
			"a caller that asked to exclude pending must not be served one: {out}"
		);
		assert!(text(&out).contains("thought not found"));
	}
}

#[cfg(test)]
mod cold_tier_filter_tests {
	use crate::base_types::{Entity, EntityKind, Source};
	use crate::mcp::tools::is_error;

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
		let (url, _server) = crate::test_support::spawn_http(app).await;
		let mut srv = crate::test_support::mcp_server_with_embed_url(&url);
		// The ranked path embeds the query itself, so this rig needs the server's
		// own client, not just the worker's.
		srv.llm = Some(crate::llm::Client::new_embed_only(&url, "test", ""));

		let dir = tempfile::tempdir().expect("tmpdir");
		let store = crate::base_store::Store::open(&dir.path().to_string_lossy()).expect("store");
		store
			.cold_put_all(&[
				spilled("cold_fact", EntityKind::Fact),
				spilled("cold_claim", EntityKind::Claim),
			])
			.expect("spill");
		srv.graph.write().set_store(std::sync::Arc::new(store));

		let ids = |out: &serde_json::Value| -> Vec<String> {
			let body: serde_json::Value =
				serde_json::from_str(&crate::test_support::tool_text(out)).expect("json body");
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
		assert!(!is_error(&out), "{out}");
		let all = ids(&out);
		assert!(
			all.contains(&"cold_fact".to_string()) && all.contains(&"cold_claim".to_string()),
			"precondition: the cold fill reaches both rows: {all:?}"
		);

		let out = srv.tool_query(&serde_json::json!({"text": "anything", "kind": "fact"}));
		assert!(!is_error(&out), "{out}");
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

#[cfg(test)]
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

// The three links item 21 was missing between a caller and the engine's
// `exclude_pending`: the schema has to advertise it, `QueryArgs` has to accept
// it, and `build_query_options` has to carry it through. A predicate the engine
// honours and no caller can set is the same as no predicate at all.
#[cfg(test)]
mod exclude_pending_surface_tests {
	use super::{build_query_options, tool_schemas, QueryArgs};

	fn args(v: serde_json::Value) -> QueryArgs {
		serde_json::from_value(v).expect("arguments deserialize")
	}

	#[test]
	fn the_query_schema_advertises_exclude_pending() {
		let props = &tool_schemas()[0]["inputSchema"]["properties"];
		assert_eq!(
			props["exclude_pending"]["type"], "boolean",
			"an undeclared filter is one no MCP host can discover: {props}"
		);
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
