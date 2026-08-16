//! Entity-level subcommands: get, list, link, forget (by id and by source),
//! degrade, promote, move — the per-thought reads and writes shared by the
//! CLI and MCP surfaces.

use base::base_types::{EntityKind, Kern, ReasonKind, Source};
use graph::graph::GraphGnn;
use graph::search::find_entity;
use retrieval::id_detail::entity_detail_by_id;
use util::{explain_relationship_prompt, short_id, truncate};

use crate::commands_route::{array_field, f64_field, route, str_field, u64_field, Routed};
use crate::{fail, hint, load_graph, with_graph, Client, Endpoint};

fn print_kern(kern: &Kern, g: &GraphGnn, depth: usize) {
	let indent = "  ".repeat(depth);
	let label = if kern.graviton_text.is_empty() {
		"[unnamed]".to_string()
	} else {
		kern.graviton_text.clone()
	};
	println!(
		"{}kern:{}  thoughts:{}  reasons:{}",
		indent,
		label,
		kern.entities.len(),
		kern.reasons.len(),
	);
	for t in kern.entities.values() {
		println!(
			"{}  [{}] {}",
			indent,
			short_id(&t.id),
			truncate(&t.text(), 72)
		);
	}
	for child_id in &kern.children {
		if let Some(child) = g.kerns.get(child_id) {
			print_kern(child, g, depth + 1);
		}
	}
}

// The detail JSON carries kinds as discriminants; the label is what the CLI has
// always printed, and an unmapped number is shown rather than guessed at.
fn entity_kind_label(n: u64) -> String {
	match u8::try_from(n).ok().and_then(EntityKind::from_u8) {
		Some(k) => format!("{k:?}"),
		None => n.to_string(),
	}
}

fn reason_kind_label(n: u64) -> String {
	match i32::try_from(n).ok().and_then(ReasonKind::from_i32) {
		Some(k) => format!("{k:?}"),
		None => n.to_string(),
	}
}

fn print_detail(v: &serde_json::Value) {
	let id = str_field(v, "id");
	println!("ID:     {id}");
	println!("Kind:   {}", entity_kind_label(u64_field(v, "kind")));
	println!("Score:  {:.4}", f64_field(v, "score"));
	println!("Access: {}", u64_field(v, "access_count"));
	println!("Kern:   {}", short_id(str_field(v, "kern")));
	if v.get("expired").and_then(serde_json::Value::as_bool) == Some(true) {
		println!(
			"Expired: retention deadline passed at {} — `kern query` no longer returns this",
			u64_field(v, "valid_until")
		);
	}
	println!("Text:   {}", str_field(v, "text"));
	if let Some(src) = v.get("source") {
		let scheme = str_field(src, "scheme");
		let object_id = str_field(src, "object_id");
		let section = str_field(src, "section");
		if !object_id.is_empty() || !section.is_empty() {
			let sect = if section.is_empty() {
				String::new()
			} else {
				format!(" \u{a7}{section}")
			};
			println!("Source: {scheme}://{object_id}{sect}");
		}
	}

	let edges = array_field(v, "edges");
	if edges.is_empty() {
		return;
	}
	println!("Edges:");
	for e in edges {
		let from = str_field(e, "from");
		let outgoing = from == id;
		println!(
			"  {} {} score={:.4} {}  {}",
			if outgoing { "->" } else { "<-" },
			reason_kind_label(u64_field(e, "kind")),
			f64_field(e, "score"),
			short_id(if outgoing { str_field(e, "to") } else { from }),
			truncate(str_field(e, "text"), 80),
		);
	}
}

// One phrasing for the one miss every per-id command shares. The layers below
// word it their own way ("thought not found" from the daemon operations and
// `graph_ops`, nothing at all from a local lookup), and a CLI that passes each
// through verbatim tells the same story four ways.
fn no_such_thought(id: &str) -> String {
	format!("no thought with id {id}")
}

// The same, for an error that may or may not BE the miss: anything else is the
// layer's own message, which is more specific than we could be.
fn per_id_error(e: &str, id: &str) -> String {
	if e.contains("not found") {
		no_such_thought(id)
	} else {
		format!("{e}: {id}")
	}
}

// Routed first for the same reason as forget: a serving daemon's graph is newer
// than anything this process can load, so a local read would print a stale
// thought — and stale evidence is the defect one step down from a lost write.
pub(crate) async fn cmd_get(cfg: &config::Config, id: &str) {
	match route("query", serde_json::json!({"id": id})).await {
		Routed::Done(v) => return print_detail(&v),
		Routed::Refused(e) => return fail("get", per_id_error(&e, id)),
		Routed::NoDaemon => {}
	}
	let g = load_graph(cfg);
	match entity_detail_by_id(&g, id) {
		Some(detail) => print_detail(&detail),
		None => fail("get", no_such_thought(id)),
	}
}

pub(crate) fn cmd_list(cfg: &config::Config) {
	let g: GraphGnn = load_graph(cfg);
	print_kern(&g.root, &g, 0);
}

// The git-porcelain over the graph's bitemporal stamps: bare `kern log` is the
// machine history (what entered memory or fell out of currency, newest first);
// `kern log <id>` is one thought's revision chain — when each revision arrived,
// where it came from, and why the older ones were superseded.
pub(crate) async fn cmd_log(cfg: &config::Config, id: Option<&str>, limit: usize) {
	let args = match id {
		Some(id) => serde_json::json!({"id": id, "limit": limit}),
		None => serde_json::json!({"limit": limit}),
	};
	match route("log", args).await {
		Routed::Done(v) => return print_log(&v),
		Routed::Refused(e) => return fail("log", per_id_error(&e, id.unwrap_or_default())),
		Routed::NoDaemon => {}
	}
	let g = load_graph(cfg);
	match ::rpc::server::log_report(&g, id, limit) {
		Ok(v) => print_log(&v),
		Err(e) => fail("log", per_id_error(&e, id.unwrap_or_default())),
	}
}

fn print_log(v: &serde_json::Value) {
	// One thought's chain: full provenance per revision, git-show shaped.
	if let Some(revs) = v.get("revisions").and_then(|r| r.as_array()) {
		for (i, rev) in revs.iter().enumerate() {
			if i > 0 {
				println!();
			}
			let status = str_field(rev, "status");
			println!(
				"thought {}  {}{}",
				short_id(str_field(rev, "id")),
				str_field(rev, "kind"),
				if status == "active" {
					String::new()
				} else {
					format!("  ({status})")
				}
			);
			println!("Date:   {}", str_field(rev, "created"));
			let gone = str_field(rev, "invalidated");
			if !gone.is_empty() {
				println!("Gone:   {gone}");
			}
			println!("Source: {}", str_field(rev, "source"));
			let why = str_field(rev, "why");
			if !why.is_empty() {
				println!("Why:    {why}");
			}
			println!();
			println!("    {}", str_field(rev, "text"));
		}
		if revs.is_empty() {
			println!("no revisions");
		}
		return;
	}
	// The machine history: one line per change, newest first.
	let events = array_field(v, "events");
	if events.is_empty() {
		println!("no history");
		return;
	}
	for ev in events {
		println!(
			"* {}  {}  {:<10}  {}/{}  {}",
			short_id(str_field(ev, "id")),
			str_field(ev, "at"),
			str_field(ev, "change"),
			str_field(ev, "kind"),
			str_field(ev, "scheme"),
			truncate(str_field(ev, "text"), 80)
		);
	}
}

// The bulk half of `forget` (RECALL_PLAN F2a): one store load, a match over
// every thought's text, then the same removal path a single-id forget uses.
// Refuses while the writer lock is held (a serving daemon), exactly like the
// offline admin commands, because an unguarded save would clobber the daemon's
// newer state. An empty `pattern` matches every thought, which is what makes
// `--source X --dry-run` a preview of the whole source.
pub(crate) fn cmd_prune(
	cfg: &config::Config,
	pattern: &str,
	source: Option<&str>,
	dry_run: bool,
	force: bool,
) {
	let (scheme, object_id) = match source {
		Some(s) => match parse_source_selector(s) {
			Ok(pair) => (Some(pair.0), Some(pair.1)),
			Err(e) => return fail("forget", e),
		},
		None => (None, None),
	};
	if let Some(who) = store::lock::holder(&cfg.data_dir) {
		fail(
			"forget",
			format!("refused — the writer lock is held by {who}"),
		);
		return hint("a daemon serving this directory? stop it first");
	}
	let mut g = load_graph(cfg);
	let (out, samples) =
		graph::graph_ops::prune_matching(&mut g, pattern, scheme, object_id, force, dry_run);
	// What the caller selected, said back to them the way they said it — the
	// counts below are meaningless without knowing what was swept.
	let selector = match (pattern.is_empty(), source) {
		(true, Some(s)) => s.to_string(),
		(false, Some(s)) => format!("\"{pattern}\" in {s}"),
		(_, None) => format!("\"{pattern}\""),
	};
	let matched = out.removed_entities + out.kept_facts;
	if matched == 0 {
		println!("nothing matched {selector}");
		return;
	}
	if dry_run {
		println!(
			"{matched} thought(s) match {selector} — {} would be removed, {} fact(s) kept{}",
			out.removed_entities,
			out.kept_facts,
			if force {
				""
			} else {
				" (rerun with --force to remove them too)"
			}
		);
		for s in samples {
			println!("  {s}");
		}
		return;
	}
	// Every match was a Fact the guard kept, so nothing moved. Saying "forgot 0"
	// here is technically true and reads as a removal that happened.
	if out.removed_entities == 0 {
		println!(
			"kept {} fact(s) matching {selector}, removed nothing — rerun with --force to remove them",
			out.kept_facts
		);
		return;
	}
	// Guarded save + snapshot refresh, mirroring the ingest write path so the
	// next process loads the post-sweep snapshots fast.
	if let Err(e) = graph::persist::save_all(&g) {
		fail("forget", format!("save failed: {e}"));
	}
	g.consolidate_disk_index();
	println!(
		"forgot {} thought(s) ({} edges) matching {selector}{}",
		out.removed_entities,
		out.removed_edges,
		if out.kept_facts > 0 {
			format!(
				"; kept {} fact(s) — rerun with --force to remove them",
				out.kept_facts
			)
		} else {
			String::new()
		}
	);
}

// Report-only by default and safe beside a running daemon (a read of a stale
// snapshot ranks yesterday's noise, which is still noise). An `--apply` is a
// write and takes the same writer-lock refusal as `prune`.
pub(crate) fn cmd_audit(
	cfg: &config::Config,
	min_score: f64,
	limit: usize,
	json: bool,
	apply: Option<&str>,
) {
	let action = match apply {
		Some(s) => match graph::graph_ops::AuditAction::parse(s) {
			Some(a) => Some(a),
			None => {
				return fail(
					"audit",
					format!("--apply takes archive or delete, got {s:?}"),
				)
			}
		},
		None => None,
	};
	if action.is_some() {
		if let Some(who) = store::lock::holder(&cfg.data_dir) {
			fail(
				"audit",
				format!("--apply refused — the writer lock is held by {who}"),
			);
			return hint("a daemon serving this directory? stop it first");
		}
	}
	let mut g = load_graph(cfg);
	let report = graph::graph_ops::audit_noise(&g, min_score, limit);
	if json && action.is_none() {
		match serde_json::to_string_pretty(&report) {
			Ok(s) => println!("{s}"),
			Err(e) => fail("audit", e),
		}
		return;
	}
	println!(
		"audit: scanned {} thought(s), {} candidate(s) at or above {min_score}",
		report.scanned,
		report.candidates.len()
	);
	for c in &report.candidates {
		println!(
			"  {:.2} {:<7} [{}] {}  {}",
			c.score,
			c.action.as_str(),
			short_id(&c.id),
			c.reasons.join(","),
			truncate(&c.preview, 72)
		);
	}
	let Some(action) = action else {
		if !report.candidates.is_empty() {
			println!("report only — rerun with --apply archive (reversible) or --apply delete");
		}
		return;
	};
	let out = graph::graph_ops::apply_audit(&mut g, min_score, action);
	if out.archived + out.deleted > 0 {
		// Guarded save + snapshot refresh, mirroring `prune`'s write path.
		if let Err(e) = graph::persist::save_all(&g) {
			fail("audit", format!("save failed: {e}"));
		}
		g.consolidate_disk_index();
	}
	println!(
		"audit --apply: archived {} (release with `kern promote <id>`), deleted {}, kept {} fact(s), kept {} secret-bearing (delete those per-id with `kern forget`)",
		out.archived, out.deleted, out.kept_facts, out.secrets_kept
	);
}

fn print_forget(id: &str, removed: u64) {
	println!("forgot {}  removed {} edges", short_id(id), removed);
}

// Routed first: while a daemon serves, its in-memory graph is newer than
// anything this process can load, so a local forget would delete from a stale
// copy and report a stale edge count.
pub(crate) async fn cmd_forget(cfg: &config::Config, id: &str) {
	match route("forget", serde_json::json!({"id": id})).await {
		Routed::Done(v) => return print_forget(id, u64_field(&v, "removed_edges")),
		Routed::Refused(e) => return fail("forget", e),
		Routed::NoDaemon => {}
	}
	with_graph(cfg, |g| match forget_entity(g, id, false) {
		Ok(removed) => print_forget(id, removed as u64),
		// The Fact guard is the common case here — everything `kern ingest`
		// writes is a Fact — and a refusal with no way forward reads as a bug.
		Err(e) if e.contains("fact") => {
			fail("forget", format!("{e}: {id}"));
			hint("facts are removed in bulk only: `kern forget --match \"<text>\" --force`");
		}
		Err(e) => fail("forget", per_id_error(e, id)),
	});
}

// The one removal path. `force` is ROADMAP item 19's deliberate bypass of local
// fact-immunity and nothing else may set it — every per-id caller passes false.
// (Implementation lives in `graph::graph_ops`; these re-exports keep the call
// sites in the `cmd_*` wrappers below unchanged.)
pub(crate) use graph::graph_ops::{
	degrade_entity_reasons, forget_by_source, forget_entity, link_entities, promote_entity,
	SourceForget,
};

fn print_promote(id: &str, promoted: bool) {
	if promoted {
		println!("promoted {}", short_id(id));
	} else {
		println!("promoted {}  (already active)", short_id(id));
	}
}

// Routed first for the same reason as `cmd_forget`: a serving daemon's graph is
// the live one, so a local promote would release the row in a stale copy that
// the daemon's next persist overwrites — the row stays held and nothing says so.
//
// Releasing a held claim is a curation decision. The socket it routes over is
// owner-only and token-authenticated; any caller holding the mcp-token may
// release one — the process boundary is the access model.
pub(crate) async fn cmd_promote(cfg: &config::Config, id: &str) {
	match route("promote", serde_json::json!({"id": id})).await {
		Routed::Done(v) => {
			let promoted = v
				.get("promoted")
				.and_then(serde_json::Value::as_bool)
				.unwrap_or(false);
			return print_promote(id, promoted);
		}
		Routed::Refused(e) => return fail("promote", per_id_error(&e, id)),
		Routed::NoDaemon => {}
	}
	with_graph(cfg, |g| match promote_entity(g, id) {
		Ok(promoted) => print_promote(id, promoted),
		Err(e) => fail("promote", per_id_error(e, id)),
	});
}

fn parse_source_selector(arg: &str) -> Result<(&'static str, &str), String> {
	let bad = || format!("--source wants <scheme>://<object_id>, got: {arg}");
	let (scheme, object_id) = arg.split_once("://").ok_or_else(bad)?;
	let scheme = Source::parse_scheme(scheme).ok_or_else(|| {
		format!("unknown source scheme: {scheme} (file, ticket, session, agent, inline)")
	})?;
	if object_id.is_empty() {
		return Err(bad());
	}
	Ok((scheme, object_id))
}

fn print_forget_source(scheme: &str, object_id: &str, out: &SourceForget) {
	println!(
		"forgot {} thoughts from {scheme}://{object_id}  removed {} edges",
		out.removed_entities, out.removed_edges,
	);
	if out.kept_facts > 0 {
		println!(
			"  kept {} fact(s) — rerun with --force to remove them",
			out.kept_facts
		);
	}
}

pub(crate) async fn cmd_forget_source(cfg: &config::Config, source: &str, force: bool) {
	let (scheme, object_id) = match parse_source_selector(source) {
		Ok(pair) => pair,
		Err(e) => return fail("forget", e),
	};
	let args = serde_json::json!({"scheme": scheme, "object_id": object_id, "force": force});
	match route("forget_by_source", args).await {
		Routed::Done(v) => {
			return print_forget_source(
				scheme,
				object_id,
				&SourceForget {
					removed_entities: u64_field(&v, "removed_entities") as usize,
					removed_edges: u64_field(&v, "removed_edges") as usize,
					kept_facts: u64_field(&v, "kept_facts") as usize,
				},
			)
		}
		Routed::Refused(e) => return fail("forget", e),
		Routed::NoDaemon => {}
	}
	with_graph(cfg, |g| {
		let out = forget_by_source(g, scheme, object_id, force);
		print_forget_source(scheme, object_id, &out);
	});
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_link(
	cfg: &config::Config,
	from: &str,
	to: &str,
	reason: &str,
	embed_url: &str,
	embed_model: &str,
	reason_url: &str,
	reason_model: &str,
) {
	let g = load_graph(cfg);
	let (from_t, _) = match find_entity(&g, from) {
		Some(pair) => pair,
		None => return fail("link", format!("no thought with id {from}")),
	};
	let (to_t, _) = match find_entity(&g, to) {
		Some(pair) => pair,
		None => return fail("link", format!("no thought with id {to}")),
	};

	let llm_client = Client::new(
		Endpoint::new(reason_url, reason_model, cfg.reason_key()),
		Endpoint::new(embed_url, embed_model, &cfg.embed.key),
	)
	.with_timeout_secs(cfg.reason.timeout_secs);
	let mut reason_text = reason.to_string();

	if reason_text.is_empty() && !reason_url.is_empty() {
		let prompt = explain_relationship_prompt(&from_t.text(), &to_t.text());
		reason_text = llm_client
			.complete(&prompt)
			.await
			.unwrap_or_default()
			.trim()
			.to_string();
	}

	let reason_embed = if !reason_text.is_empty() {
		llm_client.embed(&reason_text).await.ok()
	} else {
		None
	};

	match link_and_persist(g, cfg, from, to, reason_text, reason_embed) {
		Ok((rid, score)) => println!(
			"linked {} -> {}  edge={}  score={:.4}",
			short_id(from),
			short_id(to),
			short_id(&rid),
			score,
		),
		Err(e) => fail("link", e),
	}
}

// Takes the loaded graph by value so the stale-graph case is reachable from a
// test: the race this guards against is a commit landing between the load and
// the flush, which nothing outside `cmd_link` can interleave while the load is
// buried inside it.
fn link_and_persist(
	mut g: GraphGnn,
	cfg: &config::Config,
	from: &str,
	to: &str,
	reason_text: String,
	reason_embed: Option<Vec<f32>>,
) -> Result<(String, f64), String> {
	let linked = link_entities(&mut g, from, to, reason_text, reason_embed, 1.0)?;
	// Guarded, not `save_graph_unguarded`: this command holds no writer lock, so
	// a daemon can commit between our load and our flush. The unguarded path
	// writes the whole kern map with no epoch check and drops that commit.
	let g = std::sync::Arc::new(parking_lot::RwLock::new(g));
	crate::save_graph_guarded(&g, cfg);
	Ok(linked)
}

// `score` is the assertion's strength, NOT cosine(from, to): a deliberate link
// exists precisely to connect what content similarity cannot, so scoring it by
// endpoint similarity guarantees the edge is weakest exactly where it is the
fn print_degrade(id: &str, decayed: u64, removed: u64) {
	println!(
		"degraded {}  decayed {} edges, removed {} below threshold",
		short_id(id),
		decayed,
		removed,
	);
}

pub(crate) async fn cmd_degrade(cfg: &config::Config, id: &str) {
	match route("degrade", serde_json::json!({"query_id": id})).await {
		Routed::Done(v) => {
			return print_degrade(
				id,
				u64_field(&v, "decayed_edges"),
				u64_field(&v, "removed_edges"),
			)
		}
		Routed::Refused(e) => return fail("degrade", per_id_error(&e, id)),
		Routed::NoDaemon => {}
	}
	with_graph(cfg, |g| {
		let (_, kern_id) = match find_entity(g, id) {
			Some(pair) => pair,
			None => return fail("degrade", no_such_thought(id)),
		};
		let (decayed, removed) = degrade_entity_reasons(g, &kern_id, id);
		print_degrade(id, decayed as u64, removed as u64);
	});
}

#[cfg(test)]
#[path = "tests/commands_graph_ops_test.rs"]
mod commands_graph_ops_tests;
