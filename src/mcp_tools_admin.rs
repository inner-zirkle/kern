//! Admin tools: health, gc, gravitons, claim-kind registration — operations
//! on the daemon and its vocabulary rather than on the knowledge itself.

use serde::Deserialize;

use crate::mcp::{tool_error, tool_result_json, Server};

#[derive(Deserialize, Default)]
struct GravitonArgs {
	#[serde(default)]
	action: String,
	#[serde(default)]
	name: String,
	#[serde(default)]
	text: String,
	#[serde(default)]
	mass: Option<f64>,
}

#[derive(Deserialize)]
struct ClaimKindArgs {
	action: String,
	name: String,
	#[serde(default)]
	description: String,
	#[serde(default)]
	parent: String,
}

#[derive(Deserialize, Default)]
struct PulseArgs {
	#[serde(default)]
	strength: f64,
}

pub(crate) fn tool_schemas() -> Vec<serde_json::Value> {
	vec![
		serde_json::json!({
			"name": "health",
			"description": "Graph statistics plus degradation signals: thought/edge counts, unnamed count, tick queue depth and latency, task panics/failures, cold evictions, and the embedding-model stamp with its mismatch flag.",
			"inputSchema": {"type": "object", "properties": {}},
		}),
		serde_json::json!({
			"name": "graviton",
			"description": "Manage gravitons: multiple named focus attractors that queries and ingest gravitate toward. Routing uses effective distance = cosine distance / mass, so heavier gravitons pull harder; non-matches fall through to `generic`. action=list (default) returns gravitons with mass; action=add needs name+text and takes optional mass (default 1.0); action=remove needs name.",
			"inputSchema": {
				"type": "object",
				"properties": {
					"action": {"type": "string", "enum": ["list", "add", "remove", "rm"], "description": "list (default) | add | remove"},
					"name": {"type": "string", "description": "graviton name (required for add/remove)"},
					"text": {"type": "string", "description": "seed text embedded into the routing vector (required for add) — a phrase, a full document, or a whole message all work"},
					"mass": {"type": "number", "description": "gravitational mass (default 1.0); heavier pulls harder"},
				},
			},
		}),
		serde_json::json!({
			"name": "claim_kind",
			"description": "Register or remove a claim kind. Registered kinds extend the built-in set (preference, decision, project, fact, code-fact, reference, procedural) that transcript distillation may label claims with. An optional `parent` makes the kind a sub-kind (RDFS-style subClassOf): a `query` filtering on the parent claim kind also returns claims labelled with the sub-kind.",
			"inputSchema": {
				"type": "object",
				"required": ["action", "name"],
				"properties": {
					"action":      {"type": "string", "enum": ["add", "rm"], "description": "add or remove"},
					"name":        {"type": "string", "description": "claim kind name"},
					"description": {"type": "string", "description": "markdown description (required for add)"},
					"parent":      {"type": "string", "description": "optional parent claim kind (builtin or registered) this kind specializes; cycles and unknown parents are refused"},
				},
			},
		}),
		serde_json::json!({
			"name": "pulse",
			"description": "Trigger a pulse through the Kern tree, enqueuing cluster tasks for all kerns with thoughts.",
			"inputSchema": {
				"type": "object",
				"properties": {
					"strength": {"type": "number", "description": "pulse strength (default 1.0)"},
				},
			},
		}),
		serde_json::json!({
			"name": "gc",
			"description": "Reap empty/orphan kerns from THIS running daemon's graph (the cwd it serves) and persist, live — no need to stop the daemon. Removes the residue of unnamed-kern churn so load and retrieval stop paying for dead shards. Returns before/after kern counts and the data.mdb file size. (Deleting rows frees pages inside the file but LMDB only returns that disk to the OS on a compaction; the file shrinks on the next restart, which auto-compacts, or via offline `kern compact`.)",
			"inputSchema": {"type": "object", "properties": {}},
		}),
		serde_json::json!({
			"name": "intake_drain",
			"description": "Drain the intake queue this daemon watches, now, instead of waiting for its poll interval: every queued `.txt` transcript is distilled into claims, every other readable file is ingested whole, and whatever committed is archived into `done/`. Takes no arguments — the queue directory comes from the daemon's own config. Returns how many entries were archived.",
			"inputSchema": {"type": "object", "properties": {}},
		}),
	]
}

impl Server {
	pub(crate) fn tool_health(&self) -> serde_json::Value {
		tool_result_json(&self.health_stats())
	}

	pub(crate) fn tool_graviton(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: GravitonArgs = serde_json::from_value(args.clone()).unwrap_or_default();
		let action = if p.action.is_empty() {
			"list"
		} else {
			p.action.as_str()
		};

		match action {
			"list" => {
				let g = self.graph.read();
				let gravitons: Vec<serde_json::Value> = crate::commands_admin::graviton_rows(&g)
					.into_iter()
					.map(|r| {
						serde_json::json!({
							"name": r.name,
							"mass": r.mass,
							"thoughts": r.thoughts,
							"reasons": r.reasons,
						})
					})
					.collect();
				tool_result_json(&serde_json::json!({ "gravitons": gravitons }))
			}
			"add" => {
				if p.name.is_empty() || p.text.is_empty() {
					return tool_error("add requires name and text");
				}
				// A multi-line seed is a list of example statements: each line is
				// embedded separately and mean-pooled, which places the graviton
				// ~0.16 cosine closer to real matching claims than embedding the
				// text whole (measured — see seed_examples).
				let examples = graph::accept::seed_examples(&p.text);
				let vec = match &self.llm {
					Some(llm) => {
						let mut vecs = Vec::with_capacity(examples.len());
						for ex in &examples {
							match crate::llm::block_on_in_place(llm.embed(ex)) {
								Some(Ok(v)) => vecs.push(v),
								Some(Err(e)) => return tool_error(&format!("embed failed: {e}")),
								None => return tool_error("no tokio runtime"),
							}
						}
						match graph::accept::mean_pool(&vecs) {
							Some(v) => v,
							None => return tool_error("empty or mismatched embeddings"),
						}
					}
					None => return tool_error("no embed client configured"),
				};
				let mut g = self.graph.write();
				graph::accept::add_graviton_with_mass(&mut g, &p.name, vec, p.mass.unwrap_or(1.0));
				drop(g);
				(self.save_fn)();
				tool_result_json(&serde_json::json!({ "added": p.name }))
			}
			"remove" | "rm" => {
				if p.name.is_empty() {
					return tool_error("remove requires name");
				}
				let mut g = self.graph.write();
				let removed = graph::accept::remove_graviton(&mut g, &p.name);
				drop(g);
				if removed {
					(self.save_fn)();
					tool_result_json(&serde_json::json!({ "removed": p.name }))
				} else {
					tool_error(&format!("graviton not found: {}", p.name))
				}
			}
			_ => tool_error("action must be add, list, or remove"),
		}
	}

	pub(crate) fn tool_claim_kind(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: ClaimKindArgs = match serde_json::from_value(args.clone()) {
			Ok(v) => v,
			Err(e) => return tool_error(&format!("invalid arguments: {e}")),
		};

		match p.action.as_str() {
			"add" => {
				if p.description.is_empty() {
					return tool_error("description required for add");
				}
				let parent = (!p.parent.is_empty()).then_some(p.parent.as_str());
				let mut g = self.graph.write();
				if let Err(e) = g.root.add_claim_kind(
					&p.name,
					&p.description,
					parent,
					&crate::ingest::distill::DEFAULT_KINDS,
				) {
					return tool_error(&e);
				}
				drop(g);
				(self.save_fn)();
				tool_result_json(&serde_json::json!({"added": p.name}))
			}
			"rm" => {
				let mut g = self.graph.write();
				g.root.rm_claim_kind(&p.name);
				drop(g);
				(self.save_fn)();
				tool_result_json(&serde_json::json!({"removed": p.name}))
			}
			_ => tool_error("action must be add or rm"),
		}
	}

	// Write lock held only for the reap; no env close, so safe while serving.
	pub(crate) fn tool_gc(&self) -> serde_json::Value {
		let (before, reaped, after) = {
			let mut g = self.graph.write();
			g.gc_empty_kerns_counted()
		};
		if reaped > 0 {
			(self.save_fn)();
		}
		// LMDB keeps freed pages until a restart/`kern compact`.
		let data_bytes = self
			.graph
			.read()
			.store()
			.map(|s| s.data_file_len())
			.unwrap_or(0);
		tool_result_json(&serde_json::json!({
			"reaped": reaped,
			"before": before,
			"after": after,
			"data_mdb_bytes": data_bytes,
			"note": if data_bytes > 256 * 1024 * 1024 {
				"rows pruned live; data.mdb keeps freed pages until the next restart auto-compacts (or run `kern compact` with the daemon stopped)"
			} else {
				"clean"
			},
		}))
	}

	pub(crate) fn tool_pulse(&self, args: &serde_json::Value) -> serde_json::Value {
		let p: PulseArgs = serde_json::from_value(args.clone()).unwrap_or_default();
		let strength = if p.strength <= 0.0 { 1.0 } else { p.strength };

		let q = match &self.task_q {
			None => {
				return tool_result_json(&serde_json::json!({
					"status": "noop",
					"enqueued": 0,
					"reason": "no task queue configured; pulse requires the daemon tick queue",
				}))
			}
			Some(q) => q,
		};

		let g = self.graph.read();
		let root_id = g.root.id.clone();
		crate::tick_pulse::pulse(q, &g, &root_id, strength);
		drop(g);

		if let Some(broadcast) = &self.broadcast_pulse {
			broadcast(&root_id, strength);
		}

		tool_result_json(&serde_json::json!({"status": "pulsed", "strength": strength}))
	}
}

use std::time::{Duration, SystemTime};

impl Server {
	// Runs the same pass the daemon's poll loop runs, in the daemon. A caller
	// draining in its own process reads the same directory and archives the same
	// entries, so both distill the file and both race the archive move.
	pub(crate) fn tool_intake_drain(&self) -> serde_json::Value {
		let dir = std::env::current_dir()
			.unwrap_or_else(|_| std::path::PathBuf::from("."))
			.join(&self.cfg.intake.dir);
		let llm_fn: Option<crate::ingest::LlmFunc> = match &self.llm {
			Some(c) if c.has_reason() => Some(std::sync::Arc::new(c.complete_func())),
			_ => None,
		};
		let extra_kinds: Vec<String> = self.graph.read().root.claim_kinds.keys().cloned().collect();

		let archived = crate::llm::block_on_in_place(crate::ingest::intake::drain_now(
			&dir,
			&self.worker,
			llm_fn.as_ref(),
			&extra_kinds,
			self.cfg.ingest.dedup_threshold,
			self.cfg.intake.retention_secs,
			self.cfg.ingest.review_policy.clone(),
			Duration::from_secs(self.cfg.intake.done_retention_secs),
			SystemTime::now(),
		));
		let Some(archived) = archived else {
			return tool_error("no tokio runtime");
		};
		if archived > 0 {
			(self.save_fn)();
		}
		tool_result_json(&serde_json::json!({"archived": archived}))
	}
}

#[cfg(test)]
mod claim_kind_tests {
	use std::sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	};

	use crate::mcp::Server;

	fn make_server() -> (Server, Arc<AtomicUsize>) {
		let counter = Arc::new(AtomicUsize::new(0));
		let c2 = counter.clone();
		let mut server = crate::test_support::mcp_server();
		server.save_fn = Arc::new(move || {
			c2.fetch_add(1, Ordering::SeqCst);
		});
		(server, counter)
	}

	use crate::mcp::tools::is_error;
	use crate::test_support::tool_text as text;

	#[tokio::test]
	async fn health_stats_aggregates_entities_and_claim_kinds() {
		use base::base_types::{Entity, Kern};
		let (srv, _c) = make_server();
		{
			let mut g = srv.graph.write();
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
			g.kerns.insert("kx".into(), k);
			g.root.claim_kinds.insert("code".into(), "source".into());
		}
		let stats = srv.health_stats();
		assert_eq!(stats["claim_kinds"], 1, "root claim kind counted");
		assert_eq!(
			stats["entities"].as_u64().unwrap(),
			2,
			"both seeded entities counted"
		);
		assert!(
			stats["kerns"].as_u64().unwrap() >= 1,
			"at least the seeded kern"
		);
	}

	#[tokio::test]
	async fn health_stats_reports_queue_depth_and_task_latency() {
		use crate::tick_queue::{task, Queue, TaskKind};
		use std::time::Duration;

		let (mut srv, _c) = make_server();
		let q = Arc::new(Queue::new(8));
		assert!(q.enqueue(task(TaskKind::Cluster, "a")));
		assert!(q.enqueue(task(TaskKind::Persist, "b")));
		q.record_task_latency(Duration::from_millis(10));
		q.record_task_latency(Duration::from_millis(30));
		srv.task_q = Some(q);

		let stats = srv.health_stats();
		assert_eq!(stats["queue_depth"], 2, "both pending tasks counted");
		assert_eq!(stats["tasks_done"], 2);
		assert_eq!(stats["task_avg_ms"], 20, "lifetime mean of 10ms and 30ms");
	}

	#[tokio::test]
	async fn health_stats_reports_zeroed_queue_metrics_without_a_queue() {
		let (srv, _c) = make_server();
		let stats = srv.health_stats();
		assert_eq!(stats["queue_depth"], 0);
		assert_eq!(stats["tasks_done"], 0);
		assert_eq!(stats["task_avg_ms"], 0);
	}

	#[tokio::test]
	async fn add_inserts_claim_kind_and_calls_save() {
		let (srv, counter) = make_server();
		let out = srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "code", "description": "source code snippets"}),
		);
		assert!(!is_error(&out));
		let body: serde_json::Value = serde_json::from_str(&text(&out)).unwrap();
		assert_eq!(body["added"], "code");
		assert_eq!(counter.load(Ordering::SeqCst), 1);
		let g = srv.graph.read();
		assert_eq!(
			g.root.claim_kinds.get("code").map(String::as_str),
			Some("source code snippets")
		);
	}

	#[tokio::test]
	async fn add_empty_description_returns_error_no_save() {
		let (srv, counter) = make_server();
		let out =
			srv.tool_claim_kind(&serde_json::json!({"action": "add", "name": "code", "description": ""}));
		assert!(is_error(&out));
		assert!(text(&out).contains("description required"));
		assert_eq!(counter.load(Ordering::SeqCst), 0);
	}

	#[tokio::test]
	async fn add_missing_required_field_returns_deser_error() {
		let (srv, _) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "add"}));
		assert!(is_error(&out));
		assert!(text(&out).contains("invalid arguments"));
	}

	#[tokio::test]
	async fn rm_removes_existing_claim_kind_and_calls_save_twice() {
		let (srv, counter) = make_server();
		srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "notes", "description": "markdown notes"}),
		);
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "rm", "name": "notes"}));
		assert!(!is_error(&out));
		let body: serde_json::Value = serde_json::from_str(&text(&out)).unwrap();
		assert_eq!(body["removed"], "notes");
		assert_eq!(counter.load(Ordering::SeqCst), 2);
		let g = srv.graph.read();
		assert!(!g.root.claim_kinds.contains_key("notes"));
	}

	#[tokio::test]
	async fn rm_nonexistent_is_noop_but_still_calls_save() {
		let (srv, counter) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "rm", "name": "ghost"}));
		assert!(!is_error(&out));
		let body: serde_json::Value = serde_json::from_str(&text(&out)).unwrap();
		assert_eq!(body["removed"], "ghost");
		assert_eq!(counter.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn unknown_action_returns_error() {
		let (srv, _) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "list", "name": "x"}));
		assert!(is_error(&out));
		assert!(text(&out).contains("action must be add or rm"));
	}

	#[tokio::test]
	async fn add_with_parent_puts_kind_into_the_parents_closure() {
		let (srv, _) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({
			"action": "add", "name": "rust-fact", "description": "rust facts", "parent": "code-fact"
		}));
		assert!(!is_error(&out), "builtin parent accepted: {out}");
		let g = srv.graph.read();
		let closure = g.root.claim_kind_closure("code-fact");
		assert!(
			closure.iter().any(|k| k == "rust-fact"),
			"sub-kind reachable from the parent's closure: {closure:?}"
		);
		assert!(
			!g.root
				.claim_kind_closure("preference")
				.iter()
				.any(|k| k == "rust-fact"),
			"unrelated kind's closure stays untouched"
		);
	}

	#[tokio::test]
	async fn add_with_unknown_parent_is_refused_without_save() {
		let (srv, counter) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({
			"action": "add", "name": "x", "description": "d", "parent": "ghost"
		}));
		assert!(is_error(&out));
		assert!(text(&out).contains("unknown parent claim kind"));
		assert_eq!(
			counter.load(Ordering::SeqCst),
			0,
			"refusal persists nothing"
		);
	}

	#[tokio::test]
	async fn a_parent_edge_that_closes_a_cycle_is_refused() {
		let (srv, _) = make_server();
		srv.tool_claim_kind(&serde_json::json!({"action": "add", "name": "a", "description": "d"}));
		srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "b", "description": "d", "parent": "a"}),
		);
		let out = srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "a", "description": "d", "parent": "b"}),
		);
		assert!(is_error(&out), "a->b->a must not close: {out}");
		assert!(text(&out).contains("ancestor of itself"));
		let self_loop = srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "a", "description": "d", "parent": "a"}),
		);
		assert!(is_error(&self_loop), "self-parent refused");
	}

	#[tokio::test]
	async fn rm_drops_the_kinds_own_edge_and_its_childrens_edges() {
		let (srv, _) = make_server();
		srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "mid", "description": "d", "parent": "fact"}),
		);
		srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "leaf", "description": "d", "parent": "mid"}),
		);
		srv.tool_claim_kind(&serde_json::json!({"action": "rm", "name": "mid"}));
		let g = srv.graph.read();
		assert!(g.root.claim_kind_parents.is_empty(), "both edges gone");
		assert!(
			!g.root
				.claim_kind_closure("fact")
				.iter()
				.any(|k| k == "leaf"),
			"orphaned child floats to top level, not into the grandparent"
		);
		assert!(
			g.root.claim_kinds.contains_key("leaf"),
			"child kind itself survives"
		);
	}

	#[tokio::test]
	async fn pulse_without_a_task_queue_is_a_labeled_noop() {
		let (srv, _) = make_server();
		let out = srv.tool_pulse(&serde_json::json!({}));
		assert!(!is_error(&out));
		let body: serde_json::Value = serde_json::from_str(&text(&out)).unwrap();
		assert_eq!(body["status"], "noop");
		assert_eq!(body["enqueued"], 0);
		assert!(body["reason"].as_str().unwrap().contains("no task queue"));
	}

	#[tokio::test]
	async fn graviton_remove_not_found_errors_and_does_not_save() {
		let (srv, counter) = make_server();
		let out = srv.tool_graviton(&serde_json::json!({"action": "remove", "name": "ghost"}));
		assert!(is_error(&out));
		assert!(text(&out).contains("graviton not found"));
		assert_eq!(
			counter.load(Ordering::SeqCst),
			0,
			"no persist on a not-found remove"
		);
	}

	#[tokio::test]
	async fn graviton_list_reports_mass() {
		let (srv, _) = make_server();
		{
			let mut g = srv.graph.write();
			graph::accept::add_graviton_with_mass(&mut g, "docs", vec![1.0, 0.0], 2.5);
		}
		let out = srv.tool_graviton(&serde_json::json!({"action": "list"}));
		assert!(!is_error(&out));
		let body: serde_json::Value = serde_json::from_str(&text(&out)).unwrap();
		let gravitons = body["gravitons"].as_array().unwrap();
		assert_eq!(gravitons.len(), 1);
		assert_eq!(gravitons[0]["name"], "docs");
		assert_eq!(gravitons[0]["mass"], 2.5, "mass round-trips through list");
	}

	#[tokio::test]
	async fn graviton_list_on_empty_graph_returns_no_gravitons() {
		let (srv, _) = make_server();
		let out = srv.tool_graviton(&serde_json::json!({}));
		assert!(!is_error(&out));
		let body: serde_json::Value = serde_json::from_str(&text(&out)).unwrap();
		assert!(
			body["gravitons"].as_array().unwrap().is_empty(),
			"fresh graph has no gravitons"
		);
	}
}
