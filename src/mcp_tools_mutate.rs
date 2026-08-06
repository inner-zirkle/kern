use serde::Deserialize;

use crate::base_constants::AGENT_SOURCE;
use crate::math::clamp_confidence;
use crate::reason::move_entity;
use crate::search::find_entity;
use crate::base_types::{Scoping, Source};
use crate::util::explain_relationship_prompt;
use crate::validate::validate_conf;
use crate::ingest;

pub(crate) fn tool_schemas() -> Vec<serde_json::Value> {
	vec![
		serde_json::json!({
			"name": "ingest",
			"description": "Add text to the knowledge graph.",
			"inputSchema": {
				"type": "object",
				"required": ["text"],
				"properties": {
					"text":       {"type": "string", "description": "text to ingest"},
					"source":     {"type": "string", "description": "source system identifier"},
					"object_id":  {"type": "string", "description": "stable object identifier for update semantics"},
					"section":    {"type": "string", "description": "section within the object"},
					"author":     {"type": "string", "description": "author or origin of the content"},
					"title":      {"type": "string", "description": "human-readable title"},
					"url":        {"type": "string", "description": "URL reference"},
					"conf":       {"type": "number", "description": "confidence weight 0.0-1.0 (default 0.5)"},
					"hint": {"type": "string", "description": "free-text hint describing the content, folded into the chunking prompt"},
					"retention_secs": {"type": "integer", "description": "expire this ingest after N seconds — sets valid_until, after which retrieval drops it (0 or absent = never). On a near-duplicate the shorter of the two deadlines wins, so this can shorten an existing TTL but never extend one"},
					"user_id":    {"type": "string", "description": "multi-tenancy scope: filterable user dimension (optional, absent = global)"},
					"agent_id":   {"type": "string", "description": "multi-tenancy scope: filterable agent dimension (optional, absent = global)"},
					"session_id": {"type": "string", "description": "multi-tenancy scope: filterable session dimension (optional, absent = global)"},
					"sync":       {"type": "boolean", "description": "block until ingest completes (default false)"},
				},
			},
		}),
		serde_json::json!({
			"name": "link",
			"description": "Create a reason edge between two thoughts.",
			"inputSchema": {
				"type": "object",
				"required": ["from", "to"],
				"properties": {
					"from":   {"type": "string", "description": "source thought ID"},
					"to":     {"type": "string", "description": "target thought ID"},
					"reason": {"type": "string", "description": "reason text (LLM generates if empty)"},
				},
			},
		}),
		serde_json::json!({
			"name": "forget",
			"description": "Remove a thought and cascade-delete its edges. Facts are immune.",
			"inputSchema": {
				"type": "object",
				"required": ["id"],
				"properties": {
					"id": {"type": "string", "description": "thought ID to remove"},
				},
			},
		}),
		serde_json::json!({
			"name": "forget_by_source",
			"description": "Remove every thought ingested from one source — all of its sections — and cascade-delete their edges. Local Facts are refused unless `force` is set; a remote Fact is a peer's assertion, not durable local knowledge, and goes either way.",
			"inputSchema": {
				"type": "object",
				"required": ["scheme", "object_id"],
				"properties": {
					"scheme":    {"type": "string", "description": "source scheme: file, ticket, session, agent or inline"},
					"object_id": {"type": "string", "description": "the source's object id — file path, ticket id, session id, agent object or inline hash"},
					"force":     {"type": "boolean", "description": "also remove local Facts (default false) — the only bypass of the Fact guard"},
				},
			},
		}),
		serde_json::json!({
			"name": "degrade",
			"description": "Decrease edge scores along the retrieval path for a query.",
			"inputSchema": {
				"type": "object",
				"required": ["query_id"],
				"properties": {
					"query_id": {"type": "string", "description": "thought ID at the end of a bad retrieval path"},
				},
			},
		}),
		serde_json::json!({
			"name": "move",
			"description": "Relocate a thought to another kern, carrying its outgoing edges and restamping cross-kern references.",
			"inputSchema": {
				"type": "object",
				"required": ["id", "to_kern"],
				"properties": {
					"id":      {"type": "string", "description": "thought ID to relocate"},
					"to_kern": {"type": "string", "description": "destination kern ID"},
				},
			},
		}),
		serde_json::json!({
			"name": "promote",
			"description": "Mark a thought reviewed: release it from `pending` back to `active`, so a `query` asking for `exclude_pending` returns it again. The release half of the review lifecycle a host opens with `[ingest] review_policy`. Idempotent — promoting an already-active thought reports `promoted: false` rather than failing. Any caller holding the graph's mcp-token may promote — the process boundary is the access model.",
			"inputSchema": {
				"type": "object",
				"required": ["id"],
				"properties": {
					"id": {"type": "string", "description": "thought ID to mark reviewed"},
				},
			},
		}),
	]
}

use crate::mcp::{tool_error, tool_result_json, Server};

#[derive(Deserialize, Default)]
struct IngestArgs {
	#[serde(default)]
	text: String,
	#[serde(default)]
	source: String,
	#[serde(default)]
	object_id: String,
	#[serde(default)]
	section: String,
	#[serde(default)]
	author: String,
	#[serde(default)]
	title: String,
	#[serde(default)]
	url: String,
	#[serde(default)]
	conf: f64,
	#[serde(default)]
	hint: String,
	#[serde(default)]
	retention_secs: u64,
	#[serde(default)]
	sync: bool,
	#[serde(default)]
	user_id: Option<String>,
	#[serde(default)]
	agent_id: Option<String>,
	#[serde(default)]
	session_id: Option<String>,
}

// Caller boundary: an agent caller can mint neither Fact-kind nor Fact-confidence
// entities. Kind is derived from clamped confidence, never caller-supplied.
fn validate_ingest(p: &IngestArgs) -> Result<(), String> {
	validate_conf(p.conf).map_err(|e| e.to_string())?;
	Ok(())
}

#[derive(Deserialize)]
struct LinkArgs {
	from: String,
	to: String,
	#[serde(default)]
	reason: String,
}

#[derive(Deserialize)]
struct ForgetArgs {
	id: String,
}

#[derive(Deserialize)]
struct ForgetBySourceArgs {
	scheme: String,
	object_id: String,
	#[serde(default)]
	force: bool,
}

#[derive(Deserialize)]
struct DegradeArgs {
	query_id: String,
}

#[derive(Deserialize)]
struct MoveArgs {
	id: String,
	to_kern: String,
}

#[derive(Deserialize)]
struct PromoteArgs {
	id: String,
}

impl Server {
	pub(crate) fn tool_ingest(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: IngestArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};
		if p.text.is_empty() {
			return tool_error("text is required");
		}

		if let Err(e) = validate_ingest(&p) {
			return tool_error(&e);
		}

		let valid_until = match ingest::valid_until_from_retention(p.retention_secs) {
			Ok(v) => v,
			Err(e) => return tool_error(&e),
		};

		// MCP callers are agents; clamp against AGENT_SOURCE regardless of what
		// `p.source` claims — the caller's source string cannot escalate to USER_SOURCE trust.
		let (conf, kind) = clamp_confidence(p.conf, AGENT_SOURCE);
		let src = match p.source.as_str() {
			"" | "inline" => Source::Inline {
				hash: p.object_id,
				section: p.section,
			},
			"file" => Source::File {
				path: p.object_id,
				section: p.section,
				title: p.title,
				author: p.author,
				url: p.url,
			},
			"session" => Source::Session {
				session_id: p.object_id,
				section: p.section,
				title: p.title,
			},
			"agent" => Source::Agent {
				agent: p.source.clone(),
				object_id: p.object_id,
				title: p.title,
			},
			other => Source::Ticket {
				system: other.to_string(),
				object_id: p.object_id,
				section: p.section,
				title: p.title,
				author: p.author,
				url: p.url,
			},
		};

		let scoping = Scoping {
			user_id: p.user_id,
			agent_id: p.agent_id,
			session_id: p.session_id,
		};

		if p.sync {
			let fut = self.worker.run(
				p.text,
				src,
				kind,
				p.hint,
				conf,
				AGENT_SOURCE,
				ingest::Config {
					dedup_threshold: self.cfg.ingest.dedup_threshold,
					dedup_threshold_by_kind: self.cfg.ingest.dedup_threshold_by_kind,
					valid_until,
					review_policy: self.cfg.ingest.review_policy.clone(),
					..Default::default()
				},
				scoping.clone(),
			);
			let Some(outcome) = crate::llm::block_on_in_place(fut) else {
				return tool_error("no tokio runtime");
			};
			(self.save_fn)();
			return tool_result_json(&serde_json::json!({
				"status": outcome.status.as_str(),
				"doc_id": outcome.doc_id,
				"conf": conf,
				"kind": kind as u8,
				"total_chunks": outcome.total_chunks,
				"embedded_chunks": outcome.embedded_chunks,
				"failed_chunks": outcome.failed_chunks,
				"transient_failures": outcome.transient_failures,
				"permanent_failures": outcome.permanent_failures,
				"message": outcome.message,
			}));
		}

		// Durable ack: persist to the direct intake BEFORE acknowledging, but only
		// when the drain loop runs — an undrained intake is worse than the RAM queue.
		let drain_runs = self.cfg.intake.enabled && !self.cfg.reason_url().is_empty();
		if drain_runs {
			let direct_dir = std::env::current_dir()
				.unwrap_or_else(|_| std::path::PathBuf::from("."))
				.join(&self.cfg.intake.dir)
				.join("direct");
			let job = crate::ingest::direct::DirectJob {
				text: p.text.clone(),
				source: src.clone(),
				kind,
				hint: p.hint.clone(),
				confidence: conf,
				valid_until,
				valid_from: None,
				// Same principal the sync leg above clamps against: an MCP caller is
				// an agent whatever `p.source` claims.
				source_tag: AGENT_SOURCE.to_string(),
				scoping: scoping.clone(),
			};
			match crate::ingest::direct::intake_direct(&direct_dir, &job) {
				Ok(doc_id) => {
					return tool_result_json(&serde_json::json!({
						"status": "accepted",
						"doc_id": doc_id,
						"conf": conf,
						"kind": kind as u8,
					}));
				}
				Err(e) => {
					// Fail-open: an intake-write failure must not reject knowledge —
					// fall through to the RAM queue.
					tracing::warn!(
						target: "kern.ingest.direct",
						error = %e,
						"direct intake write failed; falling back to in-RAM enqueue"
					);
				}
			}
		}

		let Some(doc_id) = self.worker.enqueue(
			p.text,
			src,
			kind,
			p.hint,
			conf,
			AGENT_SOURCE,
			ingest::Config {
				dedup_threshold: self.cfg.ingest.dedup_threshold,
				dedup_threshold_by_kind: self.cfg.ingest.dedup_threshold_by_kind,
				valid_until,
				review_policy: self.cfg.ingest.review_policy.clone(),
				..Default::default()
			},
			scoping,
		) else {
			// Loud, not a `status` field in a success envelope: the caller has to
			// re-offer this text, and a caller that must act cannot be told quietly.
			return tool_error("ingest queue full; the text was not accepted, retry");
		};
		tool_result_json(&serde_json::json!({
			"status": "queued",
			"doc_id": doc_id,
			"conf": conf,
			"kind": kind as u8,
		}))
	}

	pub(crate) fn tool_link(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: LinkArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};

		let g = self.graph.read();
		let (from_t, _) = match find_entity(&g, &p.from) {
			Some(pair) => pair,
			None => return tool_error(&format!("from thought not found: {}", p.from)),
		};
		let (to_t, _) = match find_entity(&g, &p.to) {
			Some(pair) => pair,
			None => return tool_error(&format!("to thought not found: {}", p.to)),
		};
		drop(g);

		let mut reason_text = p.reason;
		if reason_text.is_empty() {
			if let Some(llm) = &self.llm {
				let prompt = explain_relationship_prompt(&from_t.text(), &to_t.text());
				if let Some(reply) = crate::llm::block_on_in_place(llm.complete(&prompt)) {
					reason_text = reply.unwrap_or_default().trim().to_string();
				}
			}
		}

		let reason_embed = if !reason_text.is_empty() {
			self
				.llm
				.as_ref()
				.and_then(|llm| crate::llm::block_on_in_place(llm.embed(&reason_text)))
				.and_then(Result::ok)
		} else {
			None
		};

		let mut g = self.graph.write();
		let res = crate::commands_graph_ops::link_entities(
			&mut g,
			&p.from,
			&p.to,
			reason_text,
			reason_embed,
			crate::base_constants::MAX_AI_CONFIDENCE,
		);
		drop(g);

		match res {
			Ok((rid, _)) => {
				(self.save_fn)();
				tool_result_json(&serde_json::json!({"edge_id": rid}))
			}
			Err(e) => tool_error(&e),
		}
	}

	pub(crate) fn tool_forget(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: ForgetArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		let res = crate::commands_graph_ops::forget_entity(&mut g, &p.id, false);
		drop(g);

		match res {
			Ok(removed) => {
				(self.save_fn)();
				tool_result_json(&serde_json::json!({"removed_edges": removed}))
			}
			Err("thought not found") => tool_error(&format!("thought not found: {}", p.id)),
			Err(e) => tool_error(e),
		}
	}

	// The routed half of `kern forget --source` (ROADMAP item 19). Exists so the
	// command has somewhere to route: item 9 put `forget` through the daemon, and
	// a per-source forget with no tool behind it would delete from the store
	// behind a serving daemon's back.
	pub(crate) fn tool_forget_by_source(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: ForgetBySourceArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};
		let Some(scheme) = Source::parse_scheme(&p.scheme) else {
			return tool_error(&format!("unknown source scheme: {}", p.scheme));
		};
		if p.object_id.is_empty() {
			return tool_error("object_id is required");
		}

		let mut g = self.graph.write();
		let out = crate::commands_graph_ops::forget_by_source(&mut g, scheme, &p.object_id, p.force);
		drop(g);

		if out.removed_entities > 0 {
			(self.save_fn)();
		}
		tool_result_json(&serde_json::json!({
			"removed_entities": out.removed_entities,
			"removed_edges": out.removed_edges,
			"kept_facts": out.kept_facts,
		}))
	}

	pub(crate) fn tool_degrade(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: DegradeArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		let (_, kern_id) = match find_entity(&g, &p.query_id) {
			Some(pair) => pair,
			None => return tool_error(&format!("thought not found: {}", p.query_id)),
		};

		let (decayed, removed) =
			crate::commands_graph_ops::degrade_entity_reasons(&mut g, &kern_id, &p.query_id);
		drop(g);
		(self.save_fn)();

		tool_result_json(&serde_json::json!({
			"decayed_edges": decayed,
			"removed_edges": removed,
		}))
	}

	pub(crate) fn tool_move(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: MoveArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		let (_, from_kern_id) = match find_entity(&g, &p.id) {
			Some(pair) => pair,
			None => return tool_error(&format!("thought not found: {}", p.id)),
		};

		// move_entity validates before it mutates, so a rejection here cannot have
		// left the graph half-moved — nothing to roll back, nothing to persist.
		if let Err(e) = move_entity(&mut g, &from_kern_id, &p.to_kern, &p.id) {
			return tool_error(&e.to_string());
		}
		drop(g);
		(self.save_fn)();

		tool_result_json(&serde_json::json!({
			"id": p.id,
			"from_kern": from_kern_id,
			"to_kern": p.to_kern,
		}))
	}

	// The release half of the review lifecycle. Shares `promote_entity` with the
	// CLI's no-daemon fallback so the routed and local writes cannot disagree
	// about what "reviewed" means.
	pub(crate) fn tool_promote(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: PromoteArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};

		let mut g = self.graph.write();
		// An id nothing resolves is an error, never a quiet success: a caller
		// curating a typo would otherwise be told the row was released.
		let promoted = match crate::commands_graph_ops::promote_entity(&mut g, &p.id) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("{e}: {}", p.id)),
		};
		drop(g);
		// Nothing changed on a re-promote, so nothing to persist.
		if promoted {
			(self.save_fn)();
		}

		tool_result_json(&serde_json::json!({
			"id": p.id,
			"promoted": promoted,
		}))
	}
}

#[cfg(test)]
mod tests {
	use crate::reason::add_reason;
	use crate::base_types::{Entity, EntityKind, Kern, Reason};
	use crate::mcp::Server;
	use crate::mcp::tools::is_error;

	fn make_server() -> Server {
		crate::test_support::mcp_server()
	}

	use crate::test_support::tool_text as text;
	fn body(out: &serde_json::Value) -> serde_json::Value {
		serde_json::from_str(&text(out)).expect("success body is json")
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
		assert!(!is_error(&out));
		assert_eq!(body(&out)["removed_edges"], 1, "the incident edge cascades");

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
		assert!(is_error(&out));
		assert!(text(&out).contains("cannot forget a fact"));
	}

	fn sourced(id: &str, kind: EntityKind, path: &str, section: &str) -> Entity {
		Entity {
			id: id.into(),
			kind,
			source: crate::base_types::Source::File {
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
	// `call_tool` answers "unknown tool" and sends the CLI back to writing the
	// store behind the daemon — exactly what item 19 must not do.
	#[tokio::test]
	async fn forget_by_source_dispatches_through_call_tool() {
		use crate::transport::McpServer;

		let srv = source_server();
		let res = McpServer::call_tool(
			&srv,
			"forget_by_source",
			&serde_json::json!({"scheme": "file", "object_id": "notes.md"}),
		)
		.expect("the dispatcher answers");
		assert!(!res.is_error, "{:?}", res.content);

		let body: serde_json::Value =
			serde_json::from_str(res.content[0]["text"].as_str().expect("text content"))
				.expect("the result body is json");
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
		assert!(!is_error(&out), "{}", text(&out));
		assert_eq!(body(&out)["removed_entities"], 3);
		assert_eq!(body(&out)["kept_facts"], 0);
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
		assert!(is_error(&out));
		assert!(
			text(&out).contains("unknown source scheme"),
			"{}",
			text(&out)
		);

		// An unknown *object* is a legal no-op — only the scheme is a caller error.
		let out = srv.tool_forget_by_source(
			&serde_json::json!({"scheme": "file", "object_id": "never-ingested.md"}),
		);
		assert!(!is_error(&out), "{}", text(&out));
		assert_eq!(body(&out)["removed_entities"], 0);
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
		assert!(!is_error(&out));
		assert_eq!(
			body(&out)["decayed_edges"],
			2,
			"both incident edges visited"
		);

		let g = srv.graph.read();
		let kern = g.kerns.get("kx").unwrap();
		assert_eq!(kern.reasons.len(), 1, "the sub-threshold edge is reaped");
		let r = kern.reasons.get("a->b").expect("the healthy edge survives");
		assert!(r.score_lamport > 0, "decay stamped for CRDT merge");
		let deltas = g.drain_pending_deltas();
		assert!(
			deltas
				.iter()
				.any(|d| d.object_id == "a->b" && d.target == 2),
			"decay queued for gossip"
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
		assert!(!is_error(&out));
		let edge_id = body(&out)["edge_id"].as_str().expect("edge_id").to_string();

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
		assert!(is_error(&out));
		assert!(text(&out).contains("not found"));
	}

	fn move_server() -> Server {
		let srv = make_server();
		let mut src = Kern::new("src", "");
		src
			.entities
			.insert("a".into(), crate::test_support::entity("a"));
		src
			.entities
			.insert("b".into(), crate::test_support::entity("b"));
		add_reason(&mut src, crate::test_support::edge("a", "b"));
		insert_kern(&srv, src);
		insert_kern(&srv, Kern::new("dst", ""));
		srv
	}

	#[tokio::test]
	async fn tool_move_carries_entity_and_outgoing_edges() {
		let srv = move_server();

		let out = srv.tool_move(&serde_json::json!({ "id": "a", "to_kern": "dst" }));
		assert!(!is_error(&out), "{}", text(&out));
		assert_eq!(body(&out)["from_kern"], "src");
		assert_eq!(body(&out)["to_kern"], "dst");

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
		assert!(is_error(&out));
		assert!(text(&out).contains("thought not found"), "{}", text(&out));

		let out = srv.tool_move(&serde_json::json!({ "id": "a", "to_kern": "ghost_kern" }));
		assert!(is_error(&out));
		assert!(text(&out).contains("kern not found"), "{}", text(&out));

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
		assert!(is_error(&out));
		assert!(text(&out).contains("invalid arguments"), "{}", text(&out));
	}

	// The release half of item 21. Reading the row back matters: a `promote` that
	// answered `promoted: true` without touching the entity would satisfy every
	// assertion that only inspects the envelope.
	#[tokio::test]
	async fn tool_promote_releases_a_held_row_and_is_idempotent() {
		use crate::base_types::{ReviewState, Source};

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
		assert!(!is_error(&out), "{}", text(&out));
		assert_eq!(body(&out)["promoted"], serde_json::json!(true));
		assert_eq!(
			review(),
			ReviewState::Active,
			"the row itself moved, not just the answer"
		);

		// Again: a success that changed nothing, never an error — a curator who
		// retries must not be told the release failed.
		let out = srv.tool_promote(&serde_json::json!({"id": "held"}));
		assert!(!is_error(&out), "{}", text(&out));
		assert_eq!(body(&out)["promoted"], serde_json::json!(false));
		assert_eq!(review(), ReviewState::Active);
	}

	#[tokio::test]
	async fn tool_promote_refuses_an_unknown_id_rather_than_reporting_success() {
		let srv = make_server();
		let out = srv.tool_promote(&serde_json::json!({"id": "ghost"}));
		assert!(
			is_error(&out),
			"a mistyped id must not read back as a released claim: {out}"
		);
		assert!(
			text(&out).contains("thought not found"),
			"names what failed: {}",
			text(&out)
		);
	}
}
