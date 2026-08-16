//! The `query` subcommand: recall from the graph with the full filter surface
//! and render results for a terminal reader.

use graph::search::{find_entity, search_all_unlocked};
use retrieval::id_detail::base_entity_json;
use util::{short_id, truncate};

use crate::commands_route::{array_field, f64_field, route, str_field, Routed};
use crate::{fail, hint, load_graph, Client};

pub(crate) struct QueryParams<'a> {
	pub(crate) text: &'a str,
	pub(crate) mode: &'a str,
	/// Caller's `--k`. `None` means the retrieval preset's delivery cap.
	pub(crate) k: Option<usize>,
	pub(crate) exclude_pending: bool,
	pub(crate) embed_url: &'a str,
	pub(crate) embed_model: &'a str,
}

fn print_results(v: &serde_json::Value) {
	let entities = array_field(v, "entities");
	if entities.is_empty() {
		println!("no results");
		return;
	}
	for (i, e) in entities.iter().enumerate() {
		println!(
			"{}. [{:.4}] {}  {}",
			i + 1,
			f64_field(e, "score"),
			short_id(str_field(e, "id")),
			truncate(str_field(e, "text"), 120),
		);
	}

	let chains = str_field(v, "chains");
	if !chains.trim().is_empty() {
		println!("\n--- Connections ---");
		print!("{chains}");
	}
}

// Routed before the embed call: a serving daemon owns the index this query has to
// hit, and it embeds with its own configured model — the local path is what runs
// when nothing is serving.
pub(crate) async fn cmd_query(cfg: &config::Config, params: QueryParams<'_>) {
	let QueryParams {
		text,
		mode,
		k,
		exclude_pending,
		embed_url,
		embed_model,
	} = params;
	// `k` is never left to the tool's own default: that is `seed_k`, well under
	// the delivery pool this command prints locally, so omitting it would make the
	// hit count depend on whether a daemon happens to be up.
	let k = k.unwrap_or_else(|| retrieval::score::delivery_cap(&cfg.retrieval));
	match route(
		"query",
		serde_json::json!({
			"text": text, "mode": mode, "k": k, "exclude_pending": exclude_pending,
		}),
	)
	.await
	{
		Routed::Done(v) => return print_results(&v),
		Routed::Refused(e) => return fail("query", e),
		Routed::NoDaemon => {}
	}
	let g = load_graph(cfg);
	// Retrieval is LLM-free: only the embedder is needed.
	let llm_client = Client::new_embed_only(embed_url, embed_model, &cfg.embed.key);

	let vec = match llm_client.embed(text).await {
		Ok(v) => v,
		Err(e) => return fail("query", format!("embedding the query failed: {e}")),
	};

	let mode = retrieval::seed::Mode::parse(mode);

	// `None` unless the caller asked, so the unfiltered read stays byte-for-byte
	// the path it has always taken; `exclude_pending` alone makes `is_active()`
	// true, which is what puts this on the pre-filtered ANN path.
	let opts = exclude_pending.then(|| retrieval::score::QueryOptions {
		exclude_pending: true,
		..Default::default()
	});
	let result = retrieval::query::query(&g, &cfg.retrieval, &cfg.heat, &vec, text, mode, opts);
	// No save: read-only — access/heat bumps land on cloned result entities, and
	// persisting would risk clobbering a daemon's newer on-disk state.

	// Cut to `k` here, not inside the pipeline: the walk's own pool is what the
	// routed path returns, so trimming the render is what keeps a `--k` answer
	// identical whether or not a daemon happened to be up.
	let entities: Vec<serde_json::Value> = result
		.entities
		.iter()
		.take(k)
		.map(|st| base_entity_json(&st.entity, st.score))
		.collect();
	let chains = retrieval::query::format_chains(&g, &result.path_chains);
	print_results(&serde_json::json!({"entities": entities, "chains": chains}));
}

pub(crate) async fn cmd_search(
	cfg: &config::Config,
	text: &str,
	k: usize,
	embed_url: &str,
	embed_model: &str,
) {
	let g = load_graph(cfg);
	// Reason deliberately unconfigured: pure vector retrieval never calls
	// them — do NOT "fix" these to real endpoints/credentials.
	let llm_client = Client::new_embed_only(embed_url, embed_model, &cfg.embed.key);
	let vec = match llm_client.embed(text).await {
		Ok(v) => v,
		Err(e) => return fail("query", format!("embedding the query failed: {e}")),
	};

	let hits = search_all_unlocked(&g, &vec, k);
	if hits.is_empty() {
		println!("no results");
		return;
	}
	for (i, hit) in hits.iter().enumerate() {
		let text = find_entity(&g, &hit.entity_id)
			.map(|(t, _)| truncate(&t.text(), 120))
			.unwrap_or_default();
		println!(
			"{}. [{:.4}] {}  {}",
			i + 1,
			hit.score,
			short_id(&hit.entity_id),
			text
		);
	}
}

// The cross-kern read: one hub RPC; the hub fans out to every registered
// project and merges by score. The CLI never opens a second store itself —
// dispatch stays with the daemons that own them.
pub(crate) async fn cmd_search_all(cfg: &config::Config, text: &str, k: usize, live_only: bool) {
	use transport::hub_rpc::SearchReq;

	let Some(hub) =
		crate::commands_admin::connect_hub_or_start(cfg.hub.auto_start, &cfg.log_dir()).await
	else {
		fail("query --all", "no hub running and auto-start is off");
		return hint("start one with `kern hub`");
	};

	// Put THIS project in the fan-out before asking for it.
	//
	// The hub only knows roots it has resolved or that announced themselves at
	// daemon boot, so a project that has never run one is not in the registry —
	// and "search every kern on this machine" silently skipped the one the
	// caller was standing in. That reads as an empty store, not as a missing
	// registration, which is the worst way for it to be wrong.
	//
	// Not under `--live`: that flag is the caller saying "do not wake anything",
	// and resolving spawns. There the local kern joins only if it is already up.
	if !live_only {
		if let Ok(root) = std::env::current_dir() {
			let root = config::Config::resolve_root(&root).display().to_string();
			if let Err(e) = hub
				.resolve(transport::hub_rpc::ResolveReq { root: root.clone() })
				.await
			{
				// Not fatal: the other kerns can still answer, and saying so beats
				// failing the whole search over one root.
				eprintln!("kern query --all: could not register this project ({root}): {e}");
			}
		}
	}

	let res = match hub
		.search(SearchReq {
			text: text.to_string(),
			k: k as u64,
			live_only,
		})
		.await
	{
		Ok(r) => r,
		Err(e) => return fail("query --all", e),
	};
	if !res.ok {
		return fail("query --all", res.err);
	}
	// `skipped` is the kerns that could NOT answer, so it is the wrong number to
	// report as the reach: an all-answered empty search printed "across 1 kern(s)"
	// whatever the machine held.
	if res.hits.is_empty() {
		println!("no results");
	}
	for (i, hit) in res.hits.iter().enumerate() {
		let score = hit
			.entity
			.get("score")
			.and_then(|v| v.as_f64())
			.unwrap_or(0.0);
		let id = hit.entity.get("id").and_then(|v| v.as_str()).unwrap_or("");
		let text = hit
			.entity
			.get("text")
			.and_then(|v| v.as_str())
			.unwrap_or("");
		println!(
			"{}. [{score:.4}] {}  {}  {}",
			i + 1,
			short_id(id),
			hit.root,
			truncate(text, 100)
		);
	}
	// Not a failure: the kerns that did answer are a real answer, and naming the
	// ones that did not is what keeps it from reading as the whole machine's.
	for miss in &res.skipped {
		eprintln!("kern query --all: skipped {} ({})", miss.root, miss.err);
	}
}

use std::sync::Arc;
use std::time::Instant;

use retrieval::seed::Mode;
use util::profile::{render_timeline, Profile};

use crate::Endpoint;

const TIMELINE_WIDTH: usize = 40;

const DISTILL_SAMPLE: &str = "User: The deploy failed because the config pointed at the staging \
	bucket. Assistant: Fixed — the bucket name is now anchored to the environment, so production \
	reads prod-artifacts and staging keeps its own.";

fn ms(t: Instant) -> f64 {
	t.elapsed().as_secs_f64() * 1000.0
}

fn flat(name: &str, total_ms: f64) -> Profile {
	Profile {
		name: name.to_string(),
		checkpoints: Vec::new(),
		total_ms,
	}
}

fn renamed(mut p: Profile, name: &str) -> Profile {
	p.name = name.to_string();
	p
}

// Read-only: nothing is persisted, so it is safe to run next to a daemon.
pub(crate) async fn cmd_profile(cfg: &config::Config, text: &str, no_llm: bool) {
	let mut profiles: Vec<Profile> = Vec::new();

	let t = Instant::now();
	let g = load_graph(cfg);
	profiles.push(flat("load graph", ms(t)));
	let kerns = g.kerns.len();
	let mut entities = 0usize;
	for k in g.all() {
		entities += k.entities.len();
	}

	let reason_url = cfg.reason_url().to_string();
	let llm_client = Client::new(
		Endpoint::new(&reason_url, &cfg.reason.model, cfg.reason_key()),
		Endpoint::new(&cfg.embed.url, &cfg.embed.model, &cfg.embed.key),
	)
	.with_timeout_secs(cfg.reason.timeout_secs);

	let t = Instant::now();
	let qvec = match llm_client.embed(text).await {
		Ok(v) => v,
		Err(e) => {
			fail("profile", format!("embedding the probe failed: {e}"));
			return hint(format!("is the embed endpoint up at {}?", cfg.embed.url));
		}
	};
	profiles.push(flat("embed (cold)", ms(t)));

	let t = Instant::now();
	let _ = llm_client.embed(text).await;
	profiles.push(flat("embed (warm)", ms(t)));

	let t = Instant::now();
	let hits = search_all_unlocked(&g, &qvec, 10);
	profiles.push(flat(&format!("vector search ({} hits)", hits.len()), ms(t)));

	for (mode, label) in [
		(Mode::Content, "query content (no llm)"),
		(Mode::Reason, "query reason (no llm)"),
		(Mode::Hybrid, "query hybrid (no llm)"),
	] {
		let (_, p) =
			retrieval::query::query_profiled(&g, &cfg.retrieval, &cfg.heat, &qvec, text, mode, None);
		profiles.push(renamed(p, label));
	}

	if no_llm || reason_url.is_empty() {
		if !no_llm {
			eprintln!("no reason endpoint configured; skipping llm stages");
		}
	} else {
		let llm_fn: retrieval::LlmFunc = Arc::new(llm_client.complete_func());

		let t = Instant::now();
		let claims =
			ingest::distill::distill(DISTILL_SAMPLE, &[], &*llm_fn, std::time::SystemTime::now());
		let n = claims.map(|c| c.len()).unwrap_or(0);
		profiles.push(flat(&format!("distill ({n} claims)"), ms(t)));
	}

	println!("kern profile — {kerns} kerns, {entities} entities, query: {text:?}");
	println!();
	print!("{}", render_timeline(&profiles, TIMELINE_WIDTH));
}

#[cfg(test)]
#[path = "tests/commands_query_test.rs"]
mod commands_query_tests;
