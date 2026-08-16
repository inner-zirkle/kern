//! Tests extracted from mcp_tools_admin.rs
#![allow(unused)]
use super::*;

mod claim_kind_tests {
	use std::sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	};

	use crate::server::Server;

	fn make_server() -> (Server, Arc<AtomicUsize>) {
		let counter = Arc::new(AtomicUsize::new(0));
		let c2 = counter.clone();
		let mut server = crate::test_helpers::server();
		server.save_fn = Arc::new(move || {
			c2.fetch_add(1, Ordering::SeqCst);
		});
		(server, counter)
	}

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
		use std::time::Duration;
		use tick::tick_queue::{task, Queue, TaskKind};

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
		assert!(out.is_ok());
		let body = out.unwrap();
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
		assert!(out.is_err());
		assert!(out.unwrap_err().contains("description required"));
		assert_eq!(counter.load(Ordering::SeqCst), 0);
	}

	#[tokio::test]
	async fn add_missing_required_field_returns_deser_error() {
		let (srv, _) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "add"}));
		assert!(out.is_err());
		assert!(out.unwrap_err().contains("invalid arguments"));
	}

	#[tokio::test]
	async fn rm_removes_existing_claim_kind_and_calls_save_twice() {
		let (srv, counter) = make_server();
		srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "notes", "description": "markdown notes"}),
		);
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "rm", "name": "notes"}));
		assert!(out.is_ok());
		let body = out.unwrap();
		assert_eq!(body["removed"], "notes");
		assert_eq!(counter.load(Ordering::SeqCst), 2);
		let g = srv.graph.read();
		assert!(!g.root.claim_kinds.contains_key("notes"));
	}

	#[tokio::test]
	async fn rm_nonexistent_is_noop_but_still_calls_save() {
		let (srv, counter) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "rm", "name": "ghost"}));
		assert!(out.is_ok());
		let body = out.unwrap();
		assert_eq!(body["removed"], "ghost");
		assert_eq!(counter.load(Ordering::SeqCst), 1);
	}

	#[tokio::test]
	async fn unknown_action_returns_error() {
		let (srv, _) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({"action": "list", "name": "x"}));
		assert!(out.is_err());
		assert!(out.unwrap_err().contains("action must be add or rm"));
	}

	#[tokio::test]
	async fn add_with_parent_puts_kind_into_the_parents_closure() {
		let (srv, _) = make_server();
		let out = srv.tool_claim_kind(&serde_json::json!({
			"action": "add", "name": "rust-fact", "description": "rust facts", "parent": "code-fact"
		}));
		assert!(out.is_ok(), "builtin parent accepted: {out:?}");
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
		assert!(out.is_err());
		assert!(out.unwrap_err().contains("unknown parent claim kind"));
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
		assert!(out.is_err(), "a->b->a must not close: {out:?}");
		assert!(out.unwrap_err().contains("ancestor of itself"));
		let self_loop = srv.tool_claim_kind(
			&serde_json::json!({"action": "add", "name": "a", "description": "d", "parent": "a"}),
		);
		assert!(self_loop.is_err(), "self-parent refused");
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
		assert!(out.is_ok());
		let body = out.unwrap();
		assert_eq!(body["status"], "noop");
		assert_eq!(body["enqueued"], 0);
		assert!(body["reason"].as_str().unwrap().contains("no task queue"));
	}

	#[tokio::test]
	async fn graviton_remove_not_found_errors_and_does_not_save() {
		let (srv, counter) = make_server();
		let out = srv.tool_graviton(&serde_json::json!({"action": "remove", "name": "ghost"}));
		assert!(out.is_err());
		assert!(out.unwrap_err().contains("graviton not found"));
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
		assert!(out.is_ok());
		let body = out.unwrap();
		let gravitons = body["gravitons"].as_array().unwrap();
		assert_eq!(gravitons.len(), 1);
		assert_eq!(gravitons[0]["name"], "docs");
		assert_eq!(gravitons[0]["mass"], 2.5, "mass round-trips through list");
	}

	#[tokio::test]
	async fn graviton_list_on_empty_graph_returns_no_gravitons() {
		let (srv, _) = make_server();
		let out = srv.tool_graviton(&serde_json::json!({}));
		assert!(out.is_ok());
		let body = out.unwrap();
		assert!(
			body["gravitons"].as_array().unwrap().is_empty(),
			"fresh graph has no gravitons"
		);
	}
}
