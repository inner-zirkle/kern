//! Entity-level subcommands: get, list, link, forget (by id and by source),
//! degrade, promote, move — the per-thought reads and writes shared by the
//! CLI and MCP surfaces.

use crate::mcp::tools_query::entity_detail_by_id;
use base::base_constants::{
	DEGRADE_DECAY_BASE, DEGRADE_DECAY_POW, DEGRADE_FLOOR, DEGRADE_MIN_THRESHOLD,
};
use base::base_types::{EntityKind, Kern, Reason, ReasonKind, ReviewState, Source};
use graph::graph::GraphGnn;
use graph::reason::{add_reason, remove_entity, remove_reason};
use graph::search::find_entity;
use math::{average_vec, reason_id};
use util::{explain_relationship_prompt, short_id, truncate};

use crate::commands::{load_graph, with_graph, Client, Endpoint};
use crate::commands_route::{array_field, f64_field, route, str_field, u64_field, Routed};

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

// Routed first for the same reason as forget: a serving daemon's graph is newer
// than anything this process can load, so a local read would print a stale
// thought — and stale evidence is the defect one step down from a lost write.
pub(crate) async fn cmd_get(cfg: &config::Config, id: &str) {
	match route("query", serde_json::json!({"id": id})).await {
		Routed::Done(v) => return print_detail(&v),
		Routed::Refused(e) => return eprintln!("{e}"),
		Routed::NoDaemon => {}
	}
	let g = load_graph(cfg);
	match entity_detail_by_id(&g, id) {
		Some(detail) => print_detail(&detail),
		None => eprintln!("thought not found: {id}"),
	}
}

pub(crate) fn cmd_list(cfg: &config::Config) {
	let g: GraphGnn = load_graph(cfg);
	print_kern(&g.root, &g, 0);
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
		Routed::Refused(e) => return eprintln!("{e}"),
		Routed::NoDaemon => {}
	}
	with_graph(cfg, |g| match forget_entity(g, id, false) {
		Ok(removed) => print_forget(id, removed as u64),
		Err(e) => eprintln!("{e}: {id}"),
	});
}

// The one removal path. `force` is ROADMAP item 19's deliberate bypass of local
// fact-immunity and nothing else may set it — every per-id caller passes false.
pub(crate) fn forget_entity(
	g: &mut GraphGnn,
	id: &str,
	force: bool,
) -> Result<usize, &'static str> {
	let (thought, kern_id) = find_entity(g, id).ok_or("thought not found")?;
	// A remote Fact is a peer's assertion, not durable local knowledge — forgettable.
	if thought.is_fact() && !force && !graph::merge::is_remote_kern_id(&kern_id) {
		return Err("cannot forget a fact");
	}
	let edges_before = g.kerns.get(&kern_id).map(|k| k.reasons.len()).unwrap_or(0);
	remove_entity(g, &kern_id, id, force);
	let edges_after = g.kerns.get(&kern_id).map(|k| k.reasons.len()).unwrap_or(0);
	// saturating: remove_entity only drops edges, never adds — guard against underflow.
	Ok(edges_before.saturating_sub(edges_after))
}

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
		Routed::Refused(e) => return eprintln!("{e}"),
		Routed::NoDaemon => {}
	}
	with_graph(cfg, |g| match promote_entity(g, id) {
		Ok(promoted) => print_promote(id, promoted),
		Err(e) => eprintln!("{e}: {id}"),
	});
}

// The one release path, shared by the `promote` tool and the no-daemon CLI
// fallback. Idempotent on purpose: a row that was already `Active` answers
// `false` rather than erroring, so a retried promote is not a failure — but an
// id that resolves to nothing IS one, because silently succeeding on a typo
// tells a curator the claim was released when it is still held.
pub(crate) fn promote_entity(g: &mut GraphGnn, id: &str) -> Result<bool, &'static str> {
	let (thought, kern_id) = find_entity(g, id).ok_or("thought not found")?;
	// Checked on the clone, before `get_mut`: that call bumps the mutation epoch
	// and invalidates the query cache, which a no-op promote has no business doing.
	if thought.review == ReviewState::Active {
		return Ok(false);
	}
	let entity = g
		.get_mut(&kern_id)
		.and_then(|k| k.entities.get_mut(id))
		.ok_or("thought not found")?;
	entity.review = ReviewState::Active;
	Ok(true)
}

#[derive(Default)]
pub(crate) struct SourceForget {
	pub removed_entities: usize,
	pub removed_edges: usize,
	// Local Facts the guard refused. Without this a `--source` that hits nothing
	// but Facts prints "forgot 0" and never says why `--force` was the answer.
	pub kept_facts: usize,
}

// Deleting a source in the host must cascade into the graph (ROADMAP item 19).
// The key is `(scheme, object_id)` and deliberately NOT the section: `source_id`
// hashes scheme+object+section, so keying on one would forget a single section
// of a document and leave the rest. Reaches exactly as far as `forget_entity`
// does — the resident kerns; an unloaded kern is out of both their reach.
pub(crate) fn forget_by_source(
	g: &mut GraphGnn,
	scheme: &str,
	object_id: &str,
	force: bool,
) -> SourceForget {
	// Collected first: the removal mutates every kern map we would be iterating.
	let ids: Vec<String> = g
		.all()
		.into_iter()
		.flat_map(|k| k.entities.values())
		.filter(|t| t.source.scheme() == scheme && t.source.object_id() == object_id)
		.map(|t| t.id.clone())
		.collect();

	let mut out = SourceForget::default();
	for id in ids {
		match forget_entity(g, &id, force) {
			Ok(edges) => {
				out.removed_entities += 1;
				out.removed_edges += edges;
			}
			Err("cannot forget a fact") => out.kept_facts += 1,
			// The id came out of the graph one statement ago; a miss here means a
			// duplicate id across kerns already took it. Nothing left to remove.
			Err(_) => {}
		}
	}
	out
}

// `<scheme>://<object_id>` — the URI shape `Source`'s own doc comment uses
// (`src/base/types.rs`). Everything after `://` is the raw `Source::object_id()`,
// not a parsed URI path: that is the half of the pair the graph actually stores,
// and re-deriving it from a `ticket://<system>/<id>` spelling would guess.
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

// Routed first for the same reason as `cmd_forget`: a serving daemon's graph is
// the live one, and a local delete would drop rows from a stale copy while the
// daemon kept serving the originals.
pub(crate) async fn cmd_forget_source(cfg: &config::Config, source: &str, force: bool) {
	let (scheme, object_id) = match parse_source_selector(source) {
		Ok(pair) => pair,
		Err(e) => return eprintln!("{e}"),
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
		Routed::Refused(e) => return eprintln!("{e}"),
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
		None => {
			eprintln!("from thought not found: {from}");
			return;
		}
	};
	let (to_t, _) = match find_entity(&g, to) {
		Some(pair) => pair,
		None => {
			eprintln!("to thought not found: {to}");
			return;
		}
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
		Err(e) => eprintln!("{e}"),
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
	crate::commands::save_graph_guarded(&g, cfg);
	Ok(linked)
}

// `score` is the assertion's strength, NOT cosine(from, to): a deliberate link
// exists precisely to connect what content similarity cannot, so scoring it by
// endpoint similarity guarantees the edge is weakest exactly where it is the
// only evidence. Callers pass their source's confidence (user 1.0, agent 0.95).
pub(crate) fn link_entities(
	g: &mut GraphGnn,
	from: &str,
	to: &str,
	reason_text: String,
	reason_embed: Option<Vec<f32>>,
	score: f64,
) -> Result<(String, f64), String> {
	let (from_t, from_kern_id) =
		find_entity(g, from).ok_or_else(|| format!("from thought not found: {from}"))?;
	let (to_t, _) = find_entity(g, to).ok_or_else(|| format!("to thought not found: {to}"))?;

	let vec = link_vector(reason_embed, &from_t.vector, &to_t.vector);
	let rid = reason_id(from, to, ReasonKind::Similarity, &reason_text, "");
	let r = Reason {
		id: rid.clone(),
		from: from.to_string(),
		to: to.to_string(),
		kind: ReasonKind::Similarity,
		text: reason_text,
		vector: vec.into(),
		score,
		..Default::default()
	};

	let kern = g.kerns.get_mut(&from_kern_id).ok_or_else(|| {
		format!(
			"link failed: kern {} no longer present",
			short_id(&from_kern_id)
		)
	})?;
	add_reason(kern, r);
	Ok((rid, score))
}

fn link_vector(reason_embed: Option<Vec<f32>>, from_vec: &[f32], to_vec: &[f32]) -> Vec<f32> {
	reason_embed.unwrap_or_else(|| average_vec(from_vec, to_vec))
}

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
		Routed::Refused(e) => return eprintln!("{e}"),
		Routed::NoDaemon => {}
	}
	with_graph(cfg, |g| {
		let (_, kern_id) = match find_entity(g, id) {
			Some(pair) => pair,
			None => {
				eprintln!("thought not found: {id}");
				return;
			}
		};
		let (decayed, removed) = degrade_entity_reasons(g, &kern_id, id);
		print_degrade(id, decayed as u64, removed as u64);
	});
}

pub(crate) fn degrade_entity_reasons(g: &mut GraphGnn, kern_id: &str, id: &str) -> (usize, usize) {
	let rids: Vec<String> = match g.kerns.get(kern_id) {
		Some(kern) => graph::reason::collect_reason_ids(kern, id),
		None => Vec::new(),
	};

	let mut decayed = 0usize;
	let mut removed = 0usize;
	for (i, rid) in rids.iter().enumerate() {
		let decay = DEGRADE_DECAY_BASE * DEGRADE_DECAY_POW.powi(i as i32);

		let should_remove = g
			.kerns
			.get(kern_id)
			.and_then(|kern| kern.reasons.get(rid))
			.map(|r| r.score - decay < DEGRADE_MIN_THRESHOLD)
			.unwrap_or(false);

		if should_remove {
			if let Some(kern) = g.kerns.get_mut(kern_id) {
				remove_reason(kern, rid);
			}
			// A degraded Rephrase takes its alternate wording out of the index with it.
			graph::lexical::reindex_entity(g, kern_id, id);
			removed += 1;
		} else {
			let lamport = g.bump_lamport();
			let producer = g.network_id.clone();
			if let Some(kern) = g.kerns.get_mut(kern_id) {
				if let Some(r) = kern.reasons.get_mut(rid) {
					r.score = (r.score - decay).max(DEGRADE_FLOOR);
					r.score_lamport = lamport;
					r.score_producer = producer.clone();
					let lww_value =
						bincode::serde::encode_to_vec(r.score, bincode::config::standard()).unwrap_or_default();
					g.push_delta(graph::graph::PendingDelta {
						object_id: rid.clone(),
						target: 2,
						replica: String::new(),
						value: 0,
						lamport,
						producer,
						lww_value,
					});
				}
			}
		}
		decayed += 1;
	}
	(decayed, removed)
}

#[cfg(test)]
mod tests {
	use super::*;
	use base::base_types::{Entity, Kern, Reason};

	fn edge(from: &str, to: &str, score: f64) -> Reason {
		Reason {
			from: from.into(),
			to: to.into(),
			id: format!("{from}->{to}"),
			score,
			..Default::default()
		}
	}

	#[test]
	fn degrade_decays_survivors_and_removes_below_threshold() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		// BASE=0.15 pushes a->c (0.0) below the 0.05 floor; a->b (1.0) merely decays.
		add_reason(&mut k, edge("a", "b", 1.0));
		add_reason(&mut k, edge("a", "c", 0.0));
		g.kerns.insert("kx".into(), k);

		let (decayed, removed) = degrade_entity_reasons(&mut g, "kx", "a");

		assert_eq!(decayed, 2, "both incident edges visited");
		assert_eq!(removed, 1, "the sub-threshold edge is reaped");

		let kern = g.kerns.get("kx").expect("kern present");
		assert_eq!(kern.reasons.len(), 1, "only the healthy edge remains");
		let survivor = kern.reasons.get("a->b").expect("a->b survives");
		assert!(
			survivor.score < 1.0,
			"survivor was decayed, not left untouched"
		);
		assert!(
			survivor.score >= DEGRADE_MIN_THRESHOLD,
			"survivor stays above the floor"
		);
	}

	#[test]
	fn degrade_clamps_edge_score_at_floor() {
		// Under current constants DEGRADE_MIN_THRESHOLD (0.05) > DEGRADE_FLOOR (0.0),
		// so the threshold removes an edge before the clamp can fire on it. The
		// clamp is defensive: it holds the invariant "no surviving reason score
		// is below the floor" regardless of the threshold, and becomes live the
		// moment a score arrives below the floor (e.g. via a gossip merge of a
		// pre-floor-era value) or the threshold is lowered. Pin the invariant.
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		// A survivor well above the threshold, plus a sub-threshold edge that is removed.
		add_reason(&mut k, edge("a", "b", 1.0));
		add_reason(&mut k, edge("a", "c", 0.0));
		g.kerns.insert("kx".into(), k);

		degrade_entity_reasons(&mut g, "kx", "a");

		let kern = g.kerns.get("kx").expect("kern present");
		for r in kern.reasons.values() {
			assert!(
				r.score >= DEGRADE_FLOOR,
				"surviving reason {} score {} is below the floor {}",
				r.id,
				r.score,
				DEGRADE_FLOOR
			);
		}
	}

	// `kern link` takes no writer lock, so a daemon can commit between its load
	// and its flush. The unguarded save writes the whole kern map with no epoch
	// check, so that commit vanishes — the last half of item 9 that needed no
	// auth to close.
	#[test]
	fn a_link_racing_an_external_commit_keeps_both() {
		use parking_lot::RwLock;
		use std::sync::Arc;

		use base::base_types::{mk_entity, EntityKind};

		let dir = tempfile::tempdir().unwrap();
		let cfg = config::Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};

		let g = Arc::new(RwLock::new(crate::commands::load_graph(&cfg)));
		let root_id = g.read().root.id.clone();

		let mut own = Kern::new("link-kern", &root_id);
		for id in ["a", "b"] {
			own
				.entities
				.insert(id.into(), mk_entity(id, id, 1.0, EntityKind::Claim));
		}
		g.write().kerns.insert("link-kern".into(), own);
		crate::commands::save_graph_guarded(&g, &cfg);

		// What `cmd_link` holds: loaded now, flushed only after the daemon commits.
		// That staleness is the defect — a graph loaded fresh would already carry
		// the other writer's kern and write it back by accident.
		let stale = crate::commands::load_graph(&cfg);
		crate::test_support::commit_extra_kern_via_store(&g, Kern::new("daemon-kern", &root_id));
		drop(g);

		let linked = link_and_persist(stale, &cfg, "a", "b", "because".into(), None);
		assert!(linked.is_ok(), "the link itself applies: {linked:?}");

		let disk = crate::commands::load_graph(&cfg);
		assert!(
			disk.loaded("daemon-kern").is_some(),
			"the concurrent writer's kern survived the link's flush"
		);
		let kern = disk.kerns.get("link-kern").expect("our own kern persisted");
		assert!(
			kern.reasons.values().any(|r| r.from == "a" && r.to == "b"),
			"the edge we just wrote is on disk too"
		);
	}

	#[test]
	fn degrade_on_unknown_kern_is_a_noop() {
		let mut g = GraphGnn::new();
		let (decayed, removed) = degrade_entity_reasons(&mut g, "missing", "a");
		assert_eq!((decayed, removed), (0, 0));
	}

	#[test]
	fn link_vector_prefers_the_reason_embedding() {
		let v = link_vector(
			Some(vec![1.0, 2.0, 3.0]),
			&[0.0, 0.0, 0.0],
			&[9.0, 9.0, 9.0],
		);
		assert_eq!(
			v,
			vec![1.0, 2.0, 3.0],
			"an embedded reason wins over the midpoint"
		);
	}

	#[test]
	fn link_vector_falls_back_to_endpoint_midpoint() {
		let v = link_vector(None, &[0.0, 2.0], &[4.0, 6.0]);
		assert_eq!(
			v,
			vec![2.0, 4.0],
			"no embedding -> midpoint of the two endpoints"
		);
	}

	use base::base_types::EntityKind;

	fn ent(id: &str, kind: EntityKind) -> Entity {
		Entity {
			id: id.into(),
			kind,
			..Default::default()
		}
	}

	fn graph_with(entities: &[(&str, EntityKind)], edges: &[(&str, &str)]) -> GraphGnn {
		graph_in("kx", entities, edges)
	}

	fn graph_in(kern_id: &str, entities: &[(&str, EntityKind)], edges: &[(&str, &str)]) -> GraphGnn {
		let mut g = GraphGnn::new();
		let mut k = Kern::new(kern_id, "");
		for (id, kind) in entities {
			k.entities.insert((*id).into(), ent(id, *kind));
		}
		for (from, to) in edges {
			add_reason(&mut k, edge(from, to, 1.0));
		}
		g.register(k);
		g
	}

	#[test]
	fn forget_removes_thought_and_reports_edge_delta() {
		let mut g = graph_with(
			&[
				("a", EntityKind::Claim),
				("b", EntityKind::Claim),
				("c", EntityKind::Claim),
			],
			&[("a", "b"), ("a", "c")],
		);
		let removed = forget_entity(&mut g, "a", false).expect("non-fact forget succeeds");
		assert_eq!(removed, 2, "both incident edges went with a");
		let kern = g.kerns.get("kx").expect("kern present");
		assert!(!kern.entities.contains_key("a"), "a is gone from the kern");
		assert!(kern.entities.contains_key("b"), "neighbours survive");
	}

	#[test]
	fn forget_refuses_a_fact() {
		let mut g = graph_with(&[("f", EntityKind::Fact)], &[]);
		assert_eq!(
			forget_entity(&mut g, "f", false),
			Err("cannot forget a fact")
		);
		assert!(
			g.kerns.get("kx").unwrap().entities.contains_key("f"),
			"the fact is left intact"
		);
	}

	// Without this the operator has no way to remove a peer-pinned Fact by hand.
	#[test]
	fn forget_allows_a_remote_fact() {
		let mut g = graph_in("remote-evilnet-k1", &[("f", EntityKind::Fact)], &[]);
		assert_eq!(
			forget_entity(&mut g, "f", false),
			Ok(0),
			"a remote Fact must be forgettable"
		);
		assert!(
			!g.kerns
				.get("remote-evilnet-k1")
				.unwrap()
				.entities
				.contains_key("f"),
			"the remote fact is actually gone, not just reported gone"
		);
	}

	#[test]
	fn forget_unknown_id_is_rejected_not_panicked() {
		let mut g = graph_with(&[("a", EntityKind::Claim)], &[]);
		assert_eq!(
			forget_entity(&mut g, "nope", false),
			Err("thought not found")
		);
	}

	fn file_src(path: &str, section: &str) -> Source {
		Source::File {
			path: path.into(),
			section: section.into(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		}
	}

	fn sourced(id: &str, kind: EntityKind, source: Source) -> Entity {
		Entity {
			id: id.into(),
			kind,
			source,
			..Default::default()
		}
	}

	fn graph_of(kerns: &[(&str, Vec<Entity>)]) -> GraphGnn {
		let mut g = GraphGnn::new();
		for (kern_id, entities) in kerns {
			let mut k = Kern::new(*kern_id, "");
			for e in entities {
				k.entities.insert(e.id.clone(), e.clone());
			}
			g.register(k);
		}
		g
	}

	// The point of keying on (scheme, object_id) and NOT source_id: source_id
	// hashes the section too, so a per-section key would forget one chunk of a
	// document and silently leave the rest.
	#[test]
	fn forget_by_source_takes_every_section_of_one_object() {
		let mut g = graph_of(&[(
			"kx",
			vec![
				sourced("intro", EntityKind::Claim, file_src("notes.md", "intro")),
				sourced("body", EntityKind::Claim, file_src("notes.md", "body")),
				sourced(
					"other",
					EntityKind::Claim,
					file_src("elsewhere.md", "intro"),
				),
			],
		)]);
		add_reason(g.kerns.get_mut("kx").unwrap(), edge("intro", "body", 1.0));

		let out = forget_by_source(&mut g, "file", "notes.md", false);

		assert_eq!(out.removed_entities, 2, "both sections went");
		assert_eq!(out.removed_edges, 1, "the edge between them cascaded");
		assert_eq!(out.kept_facts, 0);
		let kern = g.kerns.get("kx").expect("kern present");
		assert!(!kern.entities.contains_key("intro"));
		assert!(!kern.entities.contains_key("body"));
		assert!(
			kern.entities.contains_key("other"),
			"a different object_id is untouched"
		);
	}

	// A source's chunks do not all live in one kern — placement is by similarity,
	// so a scan of a single kern would leave half the document behind.
	#[test]
	fn forget_by_source_reaches_across_kerns() {
		let mut g = graph_of(&[
			(
				"k1",
				vec![sourced(
					"a",
					EntityKind::Claim,
					file_src("notes.md", "intro"),
				)],
			),
			(
				"k2",
				vec![sourced(
					"b",
					EntityKind::Claim,
					file_src("notes.md", "body"),
				)],
			),
		]);

		let out = forget_by_source(&mut g, "file", "notes.md", false);

		assert_eq!(out.removed_entities, 2, "both kerns were swept");
		assert!(!g.kerns.get("k1").unwrap().entities.contains_key("a"));
		assert!(!g.kerns.get("k2").unwrap().entities.contains_key("b"));
	}

	// The whole reason `force` exists — and the reason it must never be the
	// default: without it a legal source deletion leaves the Facts behind, and
	// the caller has to be told that is what happened.
	#[test]
	fn a_local_fact_survives_without_force_and_goes_with_it() {
		let entities = || {
			vec![
				sourced("f", EntityKind::Fact, file_src("notes.md", "intro")),
				sourced("c", EntityKind::Claim, file_src("notes.md", "body")),
			]
		};

		let mut g = graph_of(&[("kx", entities())]);
		let out = forget_by_source(&mut g, "file", "notes.md", false);
		assert_eq!(out.removed_entities, 1, "only the Claim went");
		assert_eq!(out.kept_facts, 1, "the refusal is reported, not swallowed");
		assert!(
			g.kerns.get("kx").unwrap().entities.contains_key("f"),
			"the local Fact is actually still there, not just reported kept"
		);

		let mut g = graph_of(&[("kx", entities())]);
		let out = forget_by_source(&mut g, "file", "notes.md", true);
		assert_eq!(out.removed_entities, 2, "force took the Fact too");
		assert_eq!(out.kept_facts, 0);
		assert!(
			!g.kerns.get("kx").unwrap().entities.contains_key("f"),
			"force removes the local Fact for real — remove_entity guards it too"
		);
	}

	// A remote Fact is a peer's assertion, not durable local knowledge, so it was
	// never behind the guard `force` lifts. Needing `--force` for one would make
	// the flag look like the price of deleting anything peer-shaped.
	#[test]
	fn a_remote_fact_goes_with_or_without_force() {
		for force in [false, true] {
			let mut g = graph_of(&[(
				"remote-evilnet-k1",
				vec![sourced(
					"f",
					EntityKind::Fact,
					file_src("notes.md", "intro"),
				)],
			)]);
			let out = forget_by_source(&mut g, "file", "notes.md", force);
			assert_eq!(out.removed_entities, 1, "force={force}");
			assert_eq!(out.kept_facts, 0, "force={force}");
			assert!(!g
				.kerns
				.get("remote-evilnet-k1")
				.unwrap()
				.entities
				.contains_key("f"));
		}
	}

	// Deleting a source the graph never ingested is a legal no-op, not an error:
	// the host deletes what it has, and kern reports what it had.
	#[test]
	fn an_unknown_source_removes_nothing_and_does_not_error() {
		let mut g = graph_of(&[(
			"kx",
			vec![sourced(
				"a",
				EntityKind::Claim,
				file_src("notes.md", "intro"),
			)],
		)]);

		let out = forget_by_source(&mut g, "file", "never-ingested.md", false);
		assert_eq!((out.removed_entities, out.removed_edges), (0, 0));
		assert_eq!(out.kept_facts, 0);
		assert!(
			g.kerns.get("kx").unwrap().entities.contains_key("a"),
			"a miss must not take the graph with it"
		);

		// Same object_id under another scheme is a different source.
		let inline = forget_by_source(&mut g, "inline", "notes.md", false);
		assert_eq!(inline.removed_entities, 0, "the scheme is half the key");
	}

	#[test]
	fn the_source_selector_is_scheme_colon_slash_slash_object_id() {
		assert_eq!(
			parse_source_selector("file:///abs/path/notes.md"),
			Ok(("file", "/abs/path/notes.md")),
			"everything after :// is the object_id, slashes and all"
		);
		assert_eq!(
			parse_source_selector("inline://deadbeef"),
			Ok(("inline", "deadbeef"))
		);

		for bad in ["notes.md", "file://", "ftp://x", "://x"] {
			assert!(
				parse_source_selector(bad).is_err(),
				"{bad} must be rejected, not guessed at"
			);
		}
	}

	// Proves the printer, not the lookup: both `kern get` paths hand it this shape,
	// so the labels and the edge direction must come back out of the JSON intact.
	#[test]
	fn detail_json_carries_everything_the_get_printer_needs() {
		let mut g = graph_with(
			&[("a", EntityKind::Question), ("b", EntityKind::Claim)],
			&[("a", "b")],
		);
		{
			let a = g
				.kerns
				.get_mut("kx")
				.unwrap()
				.entities
				.get_mut("a")
				.unwrap();
			a.set_text("the question".into());
			a.source = base::base_types::Source::Session {
				session_id: "session:sess-1".into(),
				section: "2,5".into(),
				title: String::new(),
			};
		}
		let v = entity_detail_by_id(&g, "a").expect("a resolves");

		assert_eq!(entity_kind_label(u64_field(&v, "kind")), "Question");
		assert_eq!(str_field(&v, "text"), "the question");
		assert_eq!(str_field(&v, "kern"), "kx");
		let src = v.get("source").expect("detail carries provenance");
		assert_eq!(str_field(src, "scheme"), "session");
		assert_eq!(str_field(src, "object_id"), "session:sess-1");
		assert_eq!(str_field(src, "section"), "2,5");
		let edges = array_field(&v, "edges");
		assert_eq!(edges.len(), 1);
		assert_eq!(str_field(&edges[0], "from"), "a", "edge points outward");
		assert_eq!(
			reason_kind_label(u64_field(&edges[0], "kind")),
			"Similarity"
		);
		assert!(entity_detail_by_id(&g, "nope").is_none());
	}

	// The id path has no `drop_expired` in front of it and never will — it answers
	// a named row, not "what is true now". The ranked path cannot satisfy this
	// test: it is not involved.
	#[test]
	fn the_id_path_flags_an_expired_thought_instead_of_hiding_it() {
		use std::time::{Duration, SystemTime, UNIX_EPOCH};
		let now = SystemTime::now();
		let mut g = graph_with(
			&[
				("dead", EntityKind::Fact),
				("live", EntityKind::Fact),
				("forever", EntityKind::Fact),
			],
			&[],
		);
		let deadline = now - Duration::from_secs(3600);
		let k = g.kerns.get_mut("kx").expect("kern present");
		k.entities.get_mut("dead").unwrap().valid_until = Some(deadline);
		k.entities.get_mut("live").unwrap().valid_until = Some(now + Duration::from_secs(3600));

		let dead = entity_detail_by_id(&g, "dead").expect(
			"an expired thought still resolves by id — 'not found' would lie about a \
			 row GC never collects",
		);
		assert_eq!(dead["expired"], serde_json::json!(true));
		assert_eq!(
			dead["valid_until"],
			serde_json::json!(deadline.duration_since(UNIX_EPOCH).unwrap().as_secs()),
			"the deadline travels with the flag so the caller can judge the staleness"
		);

		let live = entity_detail_by_id(&g, "live").expect("live resolves");
		assert_eq!(live["expired"], serde_json::json!(false));

		let no_ttl = entity_detail_by_id(&g, "forever").expect("no-retention thought resolves");
		assert!(
			no_ttl.get("expired").is_none() && no_ttl.get("valid_until").is_none(),
			"no retention means no keys at all, not expired=false: {no_ttl}"
		);
	}
}
