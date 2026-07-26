pub mod prompt;
pub mod resources;
pub mod sse;
pub mod tools;
mod tools_admin;
mod tools_delegate;
mod tools_events;
mod tools_intake;
mod tools_mutate;
pub(crate) mod tools_query;
mod tools_setup;

use std::io::{BufReader, Read, Write};
use std::sync::Arc;

use parking_lot::RwLock;

use serde::Serialize;
use serde_json::value::RawValue;

use crate::base::graph::GraphGnn;
use crate::config::Config;
use crate::ingest;
use crate::llm;
use crate::tick;

#[derive(Serialize)]
pub(crate) struct Response {
	jsonrpc: &'static str,
	#[serde(skip_serializing_if = "Option::is_none")]
	id: Option<Box<RawValue>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	result: Option<serde_json::Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	error: Option<RpcError>,
}

#[derive(Serialize)]
struct RpcError {
	code: i32,
	message: String,
}

pub(crate) const ERR_INVALID_REQ: i32 = -32600;
pub(crate) const ERR_NOT_FOUND: i32 = -32601;

pub type PulseBroadcast = Arc<dyn Fn(&str, f64) + Send + Sync>;

pub struct Server {
	pub graph: Arc<RwLock<GraphGnn>>,
	pub worker: Arc<ingest::Worker>,
	pub llm: Option<llm::Client>,
	pub save_fn: Arc<dyn Fn() + Send + Sync>,
	pub task_q: Option<Arc<tick::queue::Queue>>,
	pub cfg: Arc<Config>,
	pub broadcast_pulse: Option<PulseBroadcast>,
	// Epoch ms of the last real tool call (health polls excluded, or the hub's
	// own idle probe would keep every node alive forever). Seeded at boot so a
	// never-used node counts idle from startup.
	pub last_activity: Arc<std::sync::atomic::AtomicU64>,
}

impl Server {
	pub fn idle_ms(&self) -> u64 {
		let last = self
			.last_activity
			.load(std::sync::atomic::Ordering::Relaxed);
		crate::base::util::now_ms().saturating_sub(last)
	}

	pub(crate) fn touch(&self) {
		self.last_activity.store(
			crate::base::util::now_ms(),
			std::sync::atomic::Ordering::Relaxed,
		);
	}
}

#[derive(Default)]
struct TickHealth {
	queue_depth: u64,
	tasks_done: u64,
	task_avg_ms: u64,
	task_panics: u64,
	last_task_panic: Option<String>,
	task_failures: u64,
	last_task_failure: Option<String>,
}

impl TickHealth {
	fn of(q: &Arc<tick::queue::Queue>) -> Self {
		let (done, avg_ms) = q.metrics();
		let (task_panics, last_panic) = q.panics();
		let (task_failures, last_failure) = q.failures();
		Self {
			queue_depth: q.pending_count() as u64,
			tasks_done: done.max(0) as u64,
			task_avg_ms: avg_ms.max(0) as u64,
			task_panics,
			last_task_panic: last_panic.map(|p| p.to_string()),
			task_failures,
			last_task_failure: last_failure.map(|f| f.to_string()),
		}
	}
}

impl Server {
	pub fn run(&self, input: impl Read, output: impl Write) {
		let mut reader = BufReader::with_capacity(1024 * 1024, input);
		let mut output = output;
		let _ = transport::serve_rw(&mut reader, &mut output, self);
	}

	pub fn run_stdio(&self) {
		let _ = transport::serve_stdio(self);
	}

	pub(crate) fn health_stats(&self) -> serde_json::Value {
		let g = self.graph.read();
		let h = crate::base::health::graph_health_stats(&g);
		let claim_kinds = g.root.claim_kinds.len();
		let tick = self.task_q.as_ref().map(TickHealth::of).unwrap_or_default();
		serde_json::json!({
			"gravitons": h.gravitons,
			"kerns": h.kerns,
			"entities": h.entities,
			"reasons": h.reasons,
			"unnamed": h.unnamed,
			"claim_kinds": claim_kinds,
			"queue_depth": tick.queue_depth,
			"tasks_done": tick.tasks_done,
			"task_avg_ms": tick.task_avg_ms,
			"task_panics": tick.task_panics,
			"last_task_panic": tick.last_task_panic,
			"task_failures": tick.task_failures,
			"last_task_failure": tick.last_task_failure,
			"cold_evicted": h.cold_evicted,
			"embed_model": h.embed_model,
			"embed_dim": h.embed_dim,
			"embed_mismatch": h.embed_mismatch,
			"query_dim_rejected": h.query_dim_rejected,
			"below_floor_deliveries": h.below_floor_deliveries,
			"clock_skew_skips": h.clock_skew_skips,
			"ingest_dropped_chunks": h.ingest_dropped_chunks,
			"remote_cap_dropped": h.remote_cap_dropped,
			"unspilled_drops": h.unspilled_drops,
			"ingest_queue_refused": h.ingest_queue_refused,
			// 0.0 = uniform access (converged); →1.0 = one entity holds all
			// access. Resident entities only (ROADMAP item 62).
			"gini_access": h.gini_access,
			// The resident-kern cap: u64::MAX = uncapped (KERN_CAP_DISABLED).
			"max_kerns": h.max_kerns as u64,
			// Supersede chains past `SUPERSEDE_CHAIN_HOP_THRESHOLD` on one
			// `external_id` (ROADMAP item 58 trigger #1).
			"supersede_chain_depth_exceeded": h.supersede_chain_depth_exceeded,
			// Largest resident kern's entity count (ROADMAP item 83): a gauge of
			// the unbounded resident set at per-kern granularity.
			"largest_kern_entities": h.largest_kern_entities,
			// Gini over resident kern sizes (ROADMAP item 83): the distribution
			// the `largest_kern_entities` max summarises — kern-size balance.
			"gini_kern_sizes": h.gini_kern_sizes,
			// Active heat retention half-life (HeatConfig.half_life_secs; the one
			// Preset::apply sets, never a config edit). Daemon-sourced like the
			// config-derived fields — the CLI's own config is irrelevant (ROADMAP
			// item 62 `kern://health` surfacing).
			"heat_half_life_secs": self.cfg.heat.half_life_secs,
			// QBST recency half-life — the 24h ranking-freshness signal, the
			// second of item 55's two freshness signals (the heat half-life
			// above is the first). Daemon-sourced: the CLI's own config is
			// irrelevant (ROADMAP item 55 measurement half).
			"qbst_recency_half_life_secs": self.cfg.retrieval.qbst_recency_half_life_secs,
			// Active RRF config + mode blends (ROADMAP item 66 measurement half).
			// Preset-owned; surfaced daemon-sourced so an operator sees which
			// preset's retrieval is running.
			"retrieval": {
				"rrf_k": self.cfg.retrieval.rrf_k,
				"rrf_global_weight": self.cfg.retrieval.rrf_global_weight,
				"weights_content": {
					"content": self.cfg.retrieval.weights_content.content,
					"reason": self.cfg.retrieval.weights_content.reason,
					"edge": self.cfg.retrieval.weights_content.edge,
				},
				"weights_reason": {
					"content": self.cfg.retrieval.weights_reason.content,
					"reason": self.cfg.retrieval.weights_reason.reason,
					"edge": self.cfg.retrieval.weights_reason.edge,
				},
				"weights_hybrid": {
					"content": self.cfg.retrieval.weights_hybrid.content,
					"reason": self.cfg.retrieval.weights_hybrid.reason,
					"edge": self.cfg.retrieval.weights_hybrid.edge,
				},
				"seed_k": self.cfg.retrieval.seed_k,
				"mmr_enabled": self.cfg.retrieval.mmr_enabled,
				"lexical_enabled": self.cfg.retrieval.lexical_enabled,
				"pagerank_enabled": self.cfg.retrieval.pagerank_enabled,
			},
			// Active preset name (ROADMAP item 87 measurement half). Preset::apply
			// is its only writer; the name frames the heat/recency/retrieval lines.
			"preset": self.cfg.preset.as_str(),
			// Active source-trust map (ROADMAP item 20 measurement half), keyed
			// on `Source::scheme()`. Empty by default (bit-identical scoring),
			// so an unconfigured kern surfaces an empty map; an operator who sets
			// `source_trust = { file = 0.8 }` sees it. Daemon-sourced: the CLI's
			// own config is irrelevant.
			"source_trust": self.cfg.retrieval.source_trust,
		// Active ingest dedup config (ROADMAP item 48 measurement half): the
		// global `dedup_threshold` plus the per-kind `dedup_threshold_by_kind`
		// array (item 48 beside, shipped 2026-07-23). `None` falls back to the
		// global. Daemon-sourced: the CLI's own config is irrelevant.
		"ingest_dedup_threshold": self.cfg.ingest.dedup_threshold,
		"ingest_dedup_threshold_by_kind": self.cfg.ingest.dedup_threshold_by_kind,
			// This server's own worker, read directly: a gauge on the live channel,
			// not a process static like the counters `h` carries.
			"ingest_queue_depth": self.worker.queue_depth(),
			"gnn_train_refused": crate::tick::trainer::gnn_train_refused(),
			// Read straight from the client, like `gnn_train_refused` above: it is a
			// property of this process's LLM leg, not of the graph `h` describes.
			"llm_complete_failed": crate::llm::complete_failed(),
			"last_llm_complete_failure": crate::llm::last_complete_failure(),
		})
	}
}

impl transport::McpServer for Server {
	fn server_name(&self) -> &str {
		"kern"
	}
	fn server_version(&self) -> &str {
		env!("CARGO_PKG_VERSION")
	}

	fn extra_capabilities(&self) -> serde_json::Value {
		serde_json::json!({"resources": {}, "prompts": {}})
	}

	fn tools_list(&self) -> Vec<transport::ToolSchema> {
		tools::typed_tool_schemas()
	}

	fn call_tool(
		&self,
		name: &str,
		args: &serde_json::Value,
	) -> Result<transport::ToolResult, transport::McpError> {
		if name != "health" {
			self.touch();
		}
		let result = match name {
			"query" => self.tool_query(args),
			"events" => self.tool_events(args),
			"ingest" => self.tool_ingest(args),
			"link" => self.tool_link(args),
			"forget" => self.tool_forget(args),
			"forget_by_source" => self.tool_forget_by_source(args),
			"degrade" => self.tool_degrade(args),
			"move" => self.tool_move(args),
			"promote" => self.tool_promote(args),
			"health" => self.tool_health(),
			"graviton" => self.tool_graviton(args),
			"claim_kind" => self.tool_claim_kind(args),
			"pulse" => self.tool_pulse(args),
			"gc" => self.tool_gc(),
			"intake_drain" => self.tool_intake_drain(),
			"setup" => self.tool_setup(),
			"sign" => self.tool_sign(args),
			"contract_grant" => self.tool_contract_grant(args),
			// ponytail: transport for the proxy's `resources/read`, not an agent
			// tool. Absent from `tool_definitions()` on purpose so `tools/list`
			// is unchanged; a schema entry would put a second name on the surface.
			RESOURCE_READ_TOOL => encode_resource_read(resources::handle_resource_read(
				self,
				None,
				params_to_raw(args),
			)),
			_ => {
				return Ok(transport::ToolResult {
					content: vec![
						serde_json::json!({"type": "text", "text": format!("unknown tool: {name}")}),
					],
					is_error: true,
					structured_content: None,
				})
			}
		};
		Ok(value_to_tool_result(&result))
	}

	fn handle_method(
		&self,
		method: &str,
		params: serde_json::Value,
	) -> Option<Result<serde_json::Value, transport::McpError>> {
		if let Some(r) = handle_graphless_method(method, &params) {
			return Some(r);
		}
		match method {
			// The one advertised method that needs the graph, which is why the
			// proxy cannot answer it from `handle_graphless_method` and routes
			// this process through `resource_read` instead.
			"resources/read" => Some(response_to_result(resources::handle_resource_read(
				self,
				None,
				params_to_raw(&params),
			))),
			_ => None,
		}
	}
}

/// The `call_tool` name that carries `resources/read` from the proxy to the
/// daemon. Deliberately not a tool schema; see [`encode_resource_read`].
pub(crate) const RESOURCE_READ_TOOL: &str = "resource_read";

/// Every advertised MCP method that answers without touching the graph.
/// The standalone server and the `kern mcp` proxy both dispatch through this,
/// so the two surfaces cannot drift: they are not two implementations that
/// agree, they are one.
pub(crate) fn handle_graphless_method(
	method: &str,
	params: &serde_json::Value,
) -> Option<Result<serde_json::Value, transport::McpError>> {
	match method {
		"resources/list" => Some(Ok(
			serde_json::json!({"resources": resources::resource_definitions()}),
		)),
		"prompts/list" => Some(Ok(
			serde_json::json!({"prompts": prompt::prompt_definitions()}),
		)),
		"prompts/get" => Some(response_to_result(prompt::handle_prompt_get(
			None,
			params_to_raw(params),
		))),
		"ping" => Some(Ok(serde_json::json!({}))),
		_ => None,
	}
}

fn params_to_raw(params: &serde_json::Value) -> Option<Box<RawValue>> {
	RawValue::from_string(serde_json::to_string(params).unwrap_or_else(|_| "null".to_string())).ok()
}

/// `resources/read`, carried over the `call_tool` passthrough for the proxy.
/// The verdict rides the text block rather than `isError` alone, because
/// `CallToolRes` carries only `content` and `isError` and an error code that
/// does not survive the hop turns `unknown resource` into a generic failure.
pub(crate) fn encode_resource_read(resp: Response) -> serde_json::Value {
	match (resp.result, resp.error) {
		(Some(v), _) => tool_result_json(&v),
		(None, Some(e)) => {
			tool_error(&serde_json::json!({"error": {"code": e.code, "message": e.message}}).to_string())
		}
		(None, None) => tool_result_json(&serde_json::Value::Null),
	}
}

/// The inverse of [`encode_resource_read`], run by the proxy.
pub(crate) fn decode_resource_read(
	result: &transport::ToolResult,
) -> Result<serde_json::Value, transport::McpError> {
	let text = result
		.content
		.first()
		.and_then(|c| c.get("text"))
		.and_then(serde_json::Value::as_str)
		.unwrap_or_default();
	let parsed: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
	if result.is_error {
		let code = parsed
			.pointer("/error/code")
			.and_then(serde_json::Value::as_i64)
			.unwrap_or(-32000);
		let message = parsed
			.pointer("/error/message")
			.and_then(serde_json::Value::as_str)
			.unwrap_or(text)
			.to_string();
		return Err(transport::McpError::Rpc { code, message });
	}
	Ok(parsed)
}

pub(crate) fn value_to_tool_result(v: &serde_json::Value) -> transport::ToolResult {
	let is_error = v
		.get("isError")
		.and_then(serde_json::Value::as_bool)
		.unwrap_or(false);
	let content = v
		.get("content")
		.and_then(serde_json::Value::as_array)
		.cloned()
		.unwrap_or_default();
	transport::ToolResult {
		content,
		is_error,
		structured_content: None,
	}
}

fn response_to_result(resp: Response) -> Result<serde_json::Value, transport::McpError> {
	match (resp.result, resp.error) {
		(Some(v), _) => Ok(v),
		(None, Some(e)) => Err(transport::McpError::Rpc {
			code: e.code as i64,
			message: e.message,
		}),
		(None, None) => Ok(serde_json::Value::Null),
	}
}

pub(crate) fn ok(id: Option<Box<RawValue>>, result: serde_json::Value) -> Response {
	Response {
		jsonrpc: "2.0",
		id,
		result: Some(result),
		error: None,
	}
}

pub(crate) fn err_resp(id: Option<Box<RawValue>>, code: i32, msg: &str) -> Response {
	Response {
		jsonrpc: "2.0",
		id,
		result: None,
		error: Some(RpcError {
			code,
			message: msg.to_string(),
		}),
	}
}

fn tool_result(content: &str) -> serde_json::Value {
	serde_json::json!({
		"content": [{"type": "text", "text": content}],
	})
}

pub(crate) fn tool_result_json(v: &serde_json::Value) -> serde_json::Value {
	let s = serde_json::to_string(v).unwrap_or_default();
	tool_result(&s)
}

pub(crate) fn tool_error(msg: &str) -> serde_json::Value {
	serde_json::json!({
		"isError": true,
		"content": [{"type": "text", "text": msg}],
	})
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	#[test]
	fn envelope_extracts_content_array_and_error_flag() {
		let env = json!({ "content": [{ "type": "text", "text": "hi" }], "isError": true });
		let r = super::value_to_tool_result(&env);
		assert!(r.is_error);
		assert_eq!(r.content.len(), 1);
		assert_eq!(r.content[0]["text"], "hi");
		assert!(r.structured_content.is_none());
	}

	#[test]
	fn envelope_missing_fields_default_to_empty_and_ok() {
		let r = super::value_to_tool_result(&json!({}));
		assert!(!r.is_error, "missing isError defaults to false");
		assert!(r.content.is_empty(), "missing content defaults to empty");
	}

	#[test]
	fn envelope_non_array_content_falls_back_to_empty() {
		let r = super::value_to_tool_result(&json!({ "content": "oops", "isError": false }));
		assert!(
			r.content.is_empty(),
			"a non-array content is ignored, not panicked on"
		);
	}

	#[tokio::test]
	async fn health_reports_degraded_maintenance_after_a_task_panic() {
		use crate::tick::queue::{task, Queue, TaskKind};
		use std::sync::Arc;

		let mut srv = crate::test_support::mcp_server();
		let q = Arc::new(Queue::new(8));
		srv.task_q = Some(q.clone());

		let h = srv.health_stats();
		assert_eq!(h["task_panics"], 0);
		assert!(h["last_task_panic"].is_null(), "healthy queue reports null");

		q.record_task_panic(&task(TaskKind::GnnPropagate, "k1"), "adj*features");

		let h = srv.health_stats();
		assert_eq!(h["task_panics"], 1);
		assert_eq!(h["last_task_panic"], "GnnPropagate[k1]: adj*features");
	}

	#[tokio::test]
	async fn health_reports_contained_task_failures_beside_panics() {
		use crate::tick::queue::{task, Queue, TaskKind};
		use std::sync::Arc;

		let mut srv = crate::test_support::mcp_server();
		let q = Arc::new(Queue::new(8));
		srv.task_q = Some(q.clone());

		let h = srv.health_stats();
		assert_eq!(h["task_failures"], 0);
		assert!(h["last_task_failure"].is_null());

		q.record_task_failure(
			&task(TaskKind::GnnPropagate, "k1"),
			"could not sample negative edges",
		);

		let h = srv.health_stats();
		assert_eq!(h["task_failures"], 1);
		assert_eq!(
			h["last_task_failure"],
			"GnnPropagate[k1]: could not sample negative edges"
		);
		assert_eq!(h["task_panics"], 0, "a failure is not a panic");
	}

	#[tokio::test]
	async fn health_carries_the_store_signals_to_the_mcp_surface() {
		let srv = crate::test_support::mcp_server();
		let h = srv.health_stats();
		for key in [
			"cold_evicted",
			"embed_model",
			"embed_dim",
			"embed_mismatch",
			"query_dim_rejected",
			"below_floor_deliveries",
			"clock_skew_skips",
			"ingest_dropped_chunks",
			"remote_cap_dropped",
			"unspilled_drops",
			"ingest_queue_refused",
			"ingest_queue_depth",
			"gnn_train_refused",
			"llm_complete_failed",
			"last_llm_complete_failure",
		] {
			assert!(!h[key].is_null(), "{key} must reach the MCP surface");
		}
		assert_eq!(h["cold_evicted"], 0);
		assert_eq!(h["embed_mismatch"], false);
	}
}
