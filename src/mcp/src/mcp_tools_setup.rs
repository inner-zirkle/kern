//! The `setup` tool: returns wiring instructions for the calling agent with
//! already-done steps marked; kern never writes a host's config itself.

use crate::tool_result_json;

// The agent-facing installer. kern never writes into a host's config itself —
// the host layout is the agent's domain — so `setup` returns instructions and
// the current gaps, and the calling agent does the wiring. One instruction set
// instead of one plugin per host.

pub(crate) struct SetupState {
	pub gravitons: Vec<String>,
	pub thoughts: u64,
	pub claim_kinds: u64,
	pub intake_dir: String,
	pub mcp_registered: bool,
}

fn check(done: bool) -> &'static str {
	if done {
		"[done]"
	} else {
		"[todo]"
	}
}

pub(crate) fn render_setup(s: &SetupState) -> String {
	let mut out = String::new();
	out.push_str(
		"# kern setup — instructions for the calling agent\n\
		\n\
		kern is this project's persistent memory: a per-directory daemon holding a\n\
		knowledge graph. You (the agent) wire it into your host by following the\n\
		steps below. Every step is idempotent — skip anything already [done].\n\n",
	);

	out.push_str("## Current state\n\n");
	out.push_str(&format!(
		"- {} MCP registered for this project (.mcp.json)\n",
		check(s.mcp_registered)
	));
	out.push_str(&format!(
		"- {} gravitons seeded ({})\n",
		check(!s.gravitons.is_empty()),
		if s.gravitons.is_empty() {
			"none".to_string()
		} else {
			s.gravitons.join(", ")
		}
	));
	out.push_str(&format!(
		"- {} claim kinds registered ({})\n",
		check(s.claim_kinds > 0),
		s.claim_kinds
	));
	out.push_str(&format!(
		"- {} memory has content ({} thoughts)\n\n",
		check(s.thoughts > 0),
		s.thoughts
	));

	if s.gravitons.is_empty() {
		out.push_str(
			"## Seed gravitons (do this first)\n\
			\n\
			Gravitons are the focus areas ingest routes into. Call the `graviton`\n\
			tool once per area; 3-6 is right. Seed `text` with EXAMPLE STATEMENTS,\n\
			one per line, shaped like the claims that should land there — each line\n\
			is embedded separately and mean-pooled, which routes measurably better\n\
			than an abstract description. Write examples about THIS project:\n\
			\n\
			- {\"action\":\"add\",\"name\":\"decisions\",\"text\":\"We chose X over Y because <reason>.\\nWe rejected Z; <reason>.\\nDecision: <choice>, with the rationale recorded.\"}\n\
			- {\"action\":\"add\",\"name\":\"architecture\",\"text\":\"The <component> owns <responsibility>.\\nData flows from <a> to <b> via <mechanism>.\\nPersistence is <storage layer>.\"}\n\
			- {\"action\":\"add\",\"name\":\"preferences\",\"text\":\"Code style: <rule>.\\nAlways <practice> before <event>.\\nAvoid <antipattern>.\"}\n\n",
		);
	}

	out.push_str(
		"## Wire capture into your host\n\
		\n\
		kern captures nothing on its own — you feed it. Two entry points:\n\
		\n\
		- `ingest` MCP tool (preferred): store one durable claim directly.\n\
		- `.kern/intake/` drop dir: write a session transcript as a `.txt` file\n\
		  there and the daemon distills it into claims in the background.\n\
		\n\
		Install a standing rule in your host's instruction file — CLAUDE.md,\n\
		AGENTS.md, .cursorrules, or the equivalent your host reads. Detect which\n\
		applies by looking at the project root and your own configuration, then\n\
		add this block if it is not already present:\n\
		\n\
		```\n\
		## Memory (kern)\n\
		- At task start: call kern `query` with the task topic to recall prior\n\
		  decisions, preferences, and facts before deciding anything.\n\
		- At task end, and whenever a durable decision, preference, constraint,\n\
		  or hard-won fact emerges: call kern `ingest` with ONE self-contained\n\
		  statement per fact. Include the why on decisions.\n\
		- When recall returns something wrong or stale: call `degrade` with the\n\
		  query id so it stops surfacing.\n\
		```\n\
		\n\
		If your host supports lifecycle hooks (e.g. a session-end hook), also\n\
		wire one that appends the session transcript to `",
	);
	out.push_str(&s.intake_dir);
	out.push_str(
		"/<timestamp>.txt`\n\
		so nothing depends on you remembering to ingest.\n\n",
	);

	out.push_str(
		"## Verify\n\
		\n\
		1. Call `ingest` with {\"text\":\"kern setup verified for this project.\",\"sync\":true} — expect status committed.\n\
		2. Call `health` — expect `thoughts` to have increased.\n\
		3. Call `query` with {\"text\":\"kern setup\"} — expect the claim back.\n\
		\n\
		If ingest fails: the embedding endpoint is down or misconfigured — check\n\
		`.kern/kern.toml` [embed] and see the `health` embed_mismatch flag.\n\n",
	);

	out.push_str(
		"## Tune (optional)\n\
		\n\
		Memory tuning is one line in `.kern/kern.toml` — the preset owns every\n\
		heat/dedup/retrieval knob; there are no individual keys to set:\n\
		\n\
		- `preset = \"relaxed\"` — the default: keep more, deliver more, forget slower\n\
		- `preset = \"medium\"` — balanced\n\
		- `preset = \"tight\"` — aggressive dedup, faster decay, fewer but sharper results\n\n",
	);

	out.push_str(
		"## Ongoing contract\n\
		\n\
		Query before deciding, ingest after deciding, degrade what misleads.\n\
		Claims should be atomic, standalone statements — not summaries of a\n\
		whole session. kern dedupes aggressively; re-ingesting a known fact is\n\
		cheap and reinforces it.\n",
	);

	out
}

impl crate::Server {
	pub(crate) fn tool_setup(&self) -> serde_json::Value {
		let (gravitons, thoughts, claim_kinds) = {
			let g = self.graph.read();
			let h = ::health::graph_health_stats(&g);
			(
				h.gravitons,
				h.entities as u64,
				g.root.claim_kinds.len() as u64,
			)
		};
		let intake_dir = self.cfg.intake.dir.clone();
		let mcp_registered = std::env::current_dir()
			.map(|d| d.join(".mcp.json").exists())
			.unwrap_or(false);
		let state = SetupState {
			gravitons,
			thoughts,
			claim_kinds,
			intake_dir,
			mcp_registered,
		};
		tool_result_json(&serde_json::json!({ "instructions": render_setup(&state) }))
	}
}

pub(crate) fn tool_schemas() -> Vec<serde_json::Value> {
	vec![serde_json::json!({
		"name": "setup",
		"description": "Returns step-by-step instructions for the calling agent to wire kern into its host: seed gravitons, install the capture rule/hook, and verify. Idempotent — reports what is already done. Call this once when configuring a new project or a new agent host.",
		"inputSchema": {"type": "object", "properties": {}},
	})]
}

#[cfg(test)]
mod tests {
	use super::*;

	fn state(gravitons: &[&str], thoughts: u64) -> SetupState {
		SetupState {
			gravitons: gravitons.iter().map(|s| s.to_string()).collect(),
			thoughts,
			claim_kinds: 0,
			intake_dir: ".kern/intake".into(),
			mcp_registered: true,
		}
	}

	#[test]
	fn fresh_project_gets_the_seeding_step() {
		let text = render_setup(&state(&[], 0));
		assert!(text.contains("## Seed gravitons"));
		assert!(text.contains("[todo] gravitons seeded (none)"));
		assert!(text.contains("[todo] memory has content"));
	}

	#[test]
	fn seeded_project_skips_the_seeding_step() {
		let text = render_setup(&state(&["decisions", "architecture"], 12));
		assert!(!text.contains("## Seed gravitons"));
		assert!(text.contains("[done] gravitons seeded (decisions, architecture)"));
		assert!(text.contains("[done] memory has content (12 thoughts)"));
	}

	#[test]
	fn capture_and_verify_are_always_present() {
		for s in [state(&[], 0), state(&["a"], 5)] {
			let text = render_setup(&s);
			assert!(text.contains("## Wire capture into your host"));
			assert!(text.contains("## Verify"));
			assert!(text.contains(".kern/intake"), "intake dir must be inlined");
			assert!(text.contains("degrade"));
			assert!(text.contains("## Tune"), "preset tiers are offered");
		}
	}
}
