use crate::search::{find_entity, search_all_unlocked};
use crate::util::{short_id, truncate};
use crate::mcp::tools_query::base_entity_json;

use crate::commands_route::{array_field, f64_field, route, str_field, Routed};
use crate::commands::{load_graph, Client};

pub(crate) struct QueryParams<'a> {
	pub(crate) text: &'a str,
	pub(crate) mode: &'a str,
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
pub(crate) async fn cmd_query(cfg: &crate::config::Config, params: QueryParams<'_>) {
	let QueryParams {
		text,
		mode,
		exclude_pending,
		embed_url,
		embed_model,
	} = params;
	// `k` is not optional here: the tool's own default is `seed_k`, well under the
	// delivery pool this command prints locally, so leaving it off would make the
	// hit count depend on whether a daemon happens to be up.
	let k = crate::retrieval::score::delivery_cap(&cfg.retrieval);
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

	let mode = crate::retrieval::seed::Mode::parse(mode);

	// `None` unless the caller asked, so the unfiltered read stays byte-for-byte
	// the path it has always taken; `exclude_pending` alone makes `is_active()`
	// true, which is what puts this on the pre-filtered ANN path.
	let opts = exclude_pending.then(|| crate::retrieval::score::QueryOptions {
		exclude_pending: true,
		..Default::default()
	});
	let result =
		crate::retrieval::query::query(&g, &cfg.retrieval, &cfg.heat, &vec, text, mode, opts);
	// No save: read-only — access/heat bumps land on cloned result entities, and
	// persisting would risk clobbering a daemon's newer on-disk state.

	let entities: Vec<serde_json::Value> = result
		.entities
		.iter()
		.map(|st| base_entity_json(&st.entity, st.score))
		.collect();
	let chains = crate::retrieval::query::format_chains(&g, &result.path_chains);
	print_results(&serde_json::json!({"entities": entities, "chains": chains}));
}

pub(crate) async fn cmd_search(
	cfg: &crate::config::Config,
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
