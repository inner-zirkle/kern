use std::time::{Duration, SystemTime};

use crate::mcp::{tool_error, tool_result_json, Server};

pub(crate) fn tool_schemas() -> Vec<serde_json::Value> {
	vec![serde_json::json!({
		"name": "intake_drain",
		"description": "Drain the intake queue this daemon watches, now, instead of waiting for its poll interval: every queued `.txt` transcript is distilled into claims, every other readable file is ingested whole, and whatever committed is archived into `done/`. Takes no arguments — the queue directory comes from the daemon's own config. Returns how many entries were archived.",
		"inputSchema": {"type": "object", "properties": {}},
	})]
}

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
