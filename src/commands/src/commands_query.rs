//! The `query` subcommand: recall from the graph with the full filter surface
//! and render results for a terminal reader.

use graph::search::{find_entity, search_all_unlocked};
use retrieval::id_detail::base_entity_json;
use util::{short_id, truncate};

use crate::commands_route::{array_field, f64_field, route, str_field, Routed};
use crate::{load_graph, Client};

pub(crate) struct QueryParams<'a> {
	pub(crate) text: &'a str,
	pub(crate) mode: &'a str,
	pub(crate) exclude_pending: bool,
	pub(crate) source_prefix: Option<&'a str>,
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
		exclude_pending,
		source_prefix,
		embed_url,
		embed_model,
	} = params;
	// `k` is not optional here: the tool's own default is `seed_k`, well under the
	// delivery pool this command prints locally, so leaving it off would make the
	// hit count depend on whether a daemon happens to be up.
	let k = retrieval::score::delivery_cap(&cfg.retrieval);
	match route(
		"query",
		serde_json::json!({
			"text": text, "mode": mode, "k": k, "exclude_pending": exclude_pending,
		}),
	)
	.await
	{
		Routed::Done(v) => return print_results(&v),
		Routed::Refused(e) => return eprintln!("{e}"),
		Routed::NoDaemon => {}
	}
	let g = load_graph(cfg);
	// Retrieval is LLM-free: only the embedder is needed.
	let llm_client = Client::new_embed_only(embed_url, embed_model, &cfg.embed.key);

	let vec = match llm_client.embed(text).await {
		Ok(v) => v,
		Err(e) => {
			eprintln!("embed: {e}");
			return;
		}
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

	let entities: Vec<serde_json::Value> = result
		.entities
		.iter()
		.map(|st| base_entity_json(&st.entity, st.score))
		.collect();
	let entities = if let Some(prefix) = source_prefix {
		filter_entities_by_source_prefix(entities, prefix)
	} else {
		entities
	};
	let chains = retrieval::query::format_chains(&g, &result.path_chains);
	print_results(&serde_json::json!({"entities": entities, "chains": chains}));
}

fn filter_entities_by_source_prefix(
	entities: Vec<serde_json::Value>,
	prefix: &str,
) -> Vec<serde_json::Value> {
	let (scheme, obj_prefix) = match prefix.split_once("://") {
		Some((s, p)) => (s, p),
		None => (prefix, ""),
	};
	entities
		.into_iter()
		.filter(|e| {
			let src = e.get("source");
			let (src_scheme, src_obj) = match src.and_then(|s| {
				Some((
					s.get("scheme")?.as_str()?,
					s.get("object_id")?.as_str()?,
				))
			}) {
				Some(p) => p,
				None => return false,
			};
			src_scheme == scheme && (obj_prefix.is_empty() || src_obj.starts_with(obj_prefix))
		})
		.collect()
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
		Err(e) => {
			eprintln!("embed: {e}");
			return;
		}
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
			eprintln!("embed: {e} (embed endpoint up at {}?)", cfg.embed.url);
			return;
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
mod tests {
	use super::*;
	use serde_json::{json, Value};

	#[tokio::test]
	async fn cmd_profile_no_llm_path_does_not_panic() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|_body: axum::Json<Value>| async move {
				axum::Json(json!({ "embeddings": [[0.1, 0.2, 0.3]] }))
			}),
		);
		let (embed_url, _server) = test_support::spawn_http(app).await;

		let dir = std::env::temp_dir().join(format!("kern_profile_smoke_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();

		let mut cfg = config::Config {
			data_dir: dir.to_string_lossy().into_owned(),
			..Default::default()
		};
		cfg.embed.url = embed_url;

		cmd_profile(&cfg, "smoke test query", true).await;

		let _ = std::fs::remove_dir_all(&dir);
	}
}
