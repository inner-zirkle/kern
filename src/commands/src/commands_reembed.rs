//! The `reembed` subcommand: re-embed every entity with the configured model
//! and restamp the store — the recovery path for an embedding-model change.

// Daemon must be stopped: this writes the graph directly. That precondition was
// unenforceable until the writer lock existed — killing the hub does not keep it
// dead, since any client with hub auto-start respawns it, and the respawned
// hub's node then flushed its stale in-memory graph over a completed re-embed.

use std::collections::HashMap;

use math::average_vec;

use crate::{fail, hint, load_graph, save_graph_unguarded, Client};

const BATCH: usize = 64;

pub(crate) async fn cmd_reembed(cfg: &config::Config, embed_url: &str, embed_model: &str) {
	let _lock = match store::lock::acquire(&cfg.data_dir, "reembed") {
		Ok(l) => l,
		Err(e) => {
			fail("reembed", e);
			return hint(
				"stop it first (`kern hub stop`, or kill the daemon) — a re-embed racing a live writer loses the rewrite",
			);
		}
	};
	let mut g = load_graph(cfg);
	let client = Client::new_embed_only(embed_url, embed_model, &cfg.embed.key);

	let mut ids: Vec<String> = Vec::new();
	let mut texts: Vec<String> = Vec::new();
	for kern in g.kerns.values() {
		for e in kern.entities.values() {
			ids.push(e.id.clone());
			texts.push(e.text());
		}
	}
	if ids.is_empty() {
		println!("reembed: graph is empty, nothing to do");
		return;
	}
	println!("reembed: {} entities -> model '{embed_model}'", ids.len());

	let new_vecs = match embed_all(&client, &ids, &texts).await {
		Ok(v) => v,
		Err(e) => return fail("reembed", format!("aborted, graph unchanged: {e}")),
	};

	// Re-seed gnn_vector from the raw embed: a stale-dimension gnn_vector would break its index.
	for kern in g.kerns.values_mut() {
		for e in kern.entities.values_mut() {
			if let Some(v) = new_vecs.get(&e.id) {
				e.vector = v.clone().into();
				e.gnn_vector = e.vector.clone();
			}
		}
	}
	// Recompute reason-edge vectors (mean of endpoints) so the reason index matches the new dimension.
	for kern in g.kerns.values_mut() {
		for r in kern.reasons.values_mut() {
			if let (Some(fv), Some(tv)) = (new_vecs.get(&r.from), new_vecs.get(&r.to)) {
				r.vector = average_vec(fv, tv).into();
			}
		}
	}

	// Stamp the model that actually produced these vectors, not the configured
	// one. `load_graph` bound `cfg.embed.model`; saving under that would record a
	// false identity, make `health` report the wrong dimension, and mask the very
	// swap the stamp exists to catch. Only after the rewrite succeeded.
	g.set_embed_model(embed_model);
	g.rebuild_index();
	save_graph_unguarded(&g);
	println!("reembed: hot graph done ({} entities)", new_vecs.len());

	match reembed_cold(g.store(), &client).await {
		Ok(n) => {
			if n > 0 {
				println!("reembed: cold tier done ({n} entities)");
			}
			restamp(&g, embed_model, &new_vecs);
			println!("reembed: complete — model is now '{embed_model}'");
		}
		// No restamp: hot vectors are new but cold rows are old-dim, and the old
		// stamp is what keeps `health` reporting the mismatch until the re-run.
		Err(e) => {
			fail("reembed", e);
			hint(format!(
				"the hot graph is on '{embed_model}' but the cold tier still uses the old model — re-run once the embed endpoint is healthy"
			));
		}
	}
}

// `check_embed_stamp` deliberately never adopts on mismatch — a config swap must
// not rewrite the record of what produced the stored vectors. A completed
// re-embed is the one legitimate transition, so it restamps explicitly here.
fn restamp(g: &graph::graph::GraphGnn, embed_model: &str, new_vecs: &HashMap<String, Vec<f32>>) {
	let (Some(store), Some(dim)) = (g.store(), new_vecs.values().next().map(|v| v.len())) else {
		return;
	};
	let stamp = store_core::EmbedStamp {
		model: embed_model.to_string(),
		dim,
	};
	if let Err(e) = store.set_embed_stamp(&stamp) {
		fail("reembed", format!("restamp failed: {e}"));
		hint("health keeps reporting a mismatch until a re-run lands one");
	}
}

async fn embed_all(
	client: &llm::Client,
	ids: &[String],
	texts: &[String],
) -> Result<HashMap<String, Vec<f32>>, String> {
	let mut out: HashMap<String, Vec<f32>> = HashMap::with_capacity(ids.len());
	let mut done = 0usize;
	for chunk in texts.chunks(BATCH) {
		let vs = client.embed_batch(chunk).await.map_err(|e| e.to_string())?;
		if vs.len() != chunk.len() {
			return Err(format!(
				"embed returned {} vectors for {} inputs",
				vs.len(),
				chunk.len()
			));
		}
		for v in vs {
			out.insert(ids[done].clone(), v);
			done += 1;
		}
		println!("  {done}/{ids_len}", ids_len = ids.len());
	}
	Ok(out)
}

// Atomic: commits only if every batch succeeds; old-dim cold vectors silently
// drop from search otherwise.
async fn reembed_cold(
	store: Option<std::sync::Arc<store_core::Store>>,
	client: &llm::Client,
) -> Result<usize, String> {
	let Some(store) = store else { return Ok(0) };
	let mut cold = store
		.cold_all()
		.map_err(|e| format!("cold load failed: {e}; cold tier left unchanged"))?;
	if cold.is_empty() {
		return Ok(0);
	}
	let total = cold.len();
	let n_batches = total.div_ceil(BATCH);
	println!("reembed: {total} cold entities");
	let texts: Vec<String> = cold.iter().map(|e| e.text()).collect();

	for (i, chunk) in texts.chunks(BATCH).enumerate() {
		let start = i * BATCH;
		// If we bail here, every entity from this batch onward keeps its old vector.
		let stale = total - start;
		let vs = client.embed_batch(chunk).await.map_err(|e| {
			format!(
				"cold batch {}/{n_batches} embed failed ({e}); {stale} of {total} cold \
				 entities NOT re-embedded; cold tier left unchanged",
				i + 1
			)
		})?;
		if vs.len() != chunk.len() {
			return Err(format!(
				"cold batch {}/{n_batches} returned {} vectors for {} inputs; {stale} of \
				 {total} cold entities NOT re-embedded; cold tier left unchanged",
				i + 1,
				vs.len(),
				chunk.len(),
			));
		}
		for (j, v) in vs.into_iter().enumerate() {
			cold[start + j].vector = v.into();
		}
	}

	// One transaction (latest-wins per id): a crash mid-commit leaves the OLD
	// rows intact — LMDB never exposes a partial transaction.
	store
		.cold_put_all(&cold)
		.map_err(|e| format!("cold write-back failed: {e}; cold tier left unchanged"))?;
	Ok(total)
}

#[cfg(test)]
#[path = "tests/commands_reembed_test.rs"]
mod commands_reembed_tests;
