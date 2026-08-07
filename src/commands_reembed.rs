//! The `reembed` subcommand: re-embed every entity with the configured model
//! and restamp the store — the recovery path for an embedding-model change.

// Daemon must be stopped: this writes the graph directly. That precondition was
// unenforceable until the writer lock existed — killing the hub does not keep it
// dead, since a surviving `kern mcp` proxy respawns it, and the respawned hub
// then flushed its stale in-memory graph over a completed re-embed.

use std::collections::HashMap;

use math::average_vec;

use crate::commands::{load_graph, save_graph_unguarded, Client};

const BATCH: usize = 64;

pub(crate) async fn cmd_reembed(cfg: &crate::config::Config, embed_url: &str, embed_model: &str) {
	let _lock = match store::lock::acquire(&cfg.data_dir, "reembed") {
		Ok(l) => l,
		Err(e) => {
			eprintln!("reembed: {e}");
			eprintln!("  stop it first (`kern hub stop`, or kill the daemon) — a re-embed racing a live writer loses the rewrite");
			return;
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
		Err(e) => {
			eprintln!("reembed: aborted, graph unchanged: {e}");
			return;
		}
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
		Err(e) => eprintln!(
			"reembed: {e}\nreembed: hot graph is on '{embed_model}' but the cold tier still \
			 uses the old model — re-run once the embed endpoint is healthy"
		),
	}
}

// `check_embed_stamp` deliberately never adopts on mismatch — a config swap must
// not rewrite the record of what produced the stored vectors. A completed
// re-embed is the one legitimate transition, so it restamps explicitly here.
fn restamp(g: &crate::graph::GraphGnn, embed_model: &str, new_vecs: &HashMap<String, Vec<f32>>) {
	let (Some(store), Some(dim)) = (g.store(), new_vecs.values().next().map(|v| v.len())) else {
		return;
	};
	let stamp = store::base_store::EmbedStamp {
		model: embed_model.to_string(),
		dim,
	};
	if let Err(e) = store.set_embed_stamp(&stamp) {
		eprintln!("reembed: restamp failed ({e}) — health keeps reporting a mismatch; re-run");
	}
}

async fn embed_all(
	client: &crate::llm::Client,
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
	store: Option<std::sync::Arc<store::base_store::Store>>,
	client: &crate::llm::Client,
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
mod tests {
	use super::*;

	#[tokio::test]
	async fn a_completed_reembed_restamps_the_store_with_the_new_model() {
		use store::base_store::{EmbedCheck, EmbedStamp, Store};
		use base::base_types::Entity;

		// Fake embed endpoint: one 2-dim vector per input, any batch size.
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|body: axum::Json<serde_json::Value>| async move {
				let n = body["input"].as_array().map_or(1, |a| a.len());
				let vecs: Vec<Vec<f64>> = (0..n).map(|_| vec![0.1, 0.2]).collect();
				axum::Json(serde_json::json!({ "embeddings": vecs }))
			}),
		);
		let (url, server) = crate::test_support::spawn_http(app).await;

		let dir = tempfile::tempdir().unwrap();
		let mut cfg = crate::config::Config::default_in(dir.path());
		cfg.embed.model = "new-model".into();

		// A store holding one 3-dim entity, stamped with the model that made it.
		{
			let store = std::sync::Arc::new(Store::open(&cfg.data_dir).unwrap());
			let mut g = crate::graph::GraphGnn::new();
			g.data_dir = cfg.data_dir.clone();
			let mut child = base::base_types::Kern::new("k", &g.root.id);
			child.entities.insert(
				"e1".into(),
				Entity {
					id: "e1".into(),
					vector: vec![9.0, 9.0, 9.0].into(),
					..Default::default()
				},
			);
			g.root.children.push("k".to_string());
			g.kerns.insert("k".into(), child);
			crate::persist::save_graph_into(&store, &g).unwrap();
			store
				.set_embed_stamp(&EmbedStamp {
					model: "old-model".into(),
					dim: 3,
				})
				.unwrap();
		}

		cmd_reembed(&cfg, &url, "new-model").await;

		let store = Store::open(&cfg.data_dir).unwrap();
		let verdict = store
			.check_embed_stamp(&EmbedStamp {
				model: "new-model".into(),
				dim: 2,
			})
			.unwrap();
		assert_eq!(
			verdict,
			EmbedCheck::Match,
			"the stamp must record the model that now owns every stored vector"
		);
		assert!(!store.embed_mismatch(), "restamp clears the mismatch flag");
		server.abort();
	}

	#[tokio::test]
	async fn embed_all_errs_when_server_returns_a_mismatched_vector_count() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|| async {
				axum::Json(serde_json::json!({ "embeddings": [[0.1, 0.2, 0.3]] }))
			}),
		);
		let (url, server) = crate::test_support::spawn_http(app).await;

		let client = crate::llm::Client::new_embed_only(&url, "test-model", "");
		let ids = vec!["a".to_string(), "b".to_string()];
		let texts = vec!["alpha".to_string(), "beta".to_string()];

		let err = embed_all(&client, &ids, &texts)
			.await
			.expect_err("a short vector count must abort the re-embed");
		assert!(
			err.contains("1 vectors for 2 inputs"),
			"the count mismatch is surfaced verbatim, got: {err}",
		);

		server.abort();
	}

	#[tokio::test]
	async fn reembed_cold_reports_stale_count_and_leaves_the_tier_unchanged_on_failure() {
		use store::base_store::Store;
		use base::base_types::Entity;

		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|| async {
				axum::Json(serde_json::json!({ "embeddings": [[0.5, 0.5]] }))
			}),
		);
		let (url, server) = crate::test_support::spawn_http(app).await;

		let dir = tempfile::tempdir().unwrap();
		let store = std::sync::Arc::new(Store::open(&dir.path().to_string_lossy()).unwrap());
		let old = vec![9.0, 9.0];
		let seed = vec![
			Entity {
				id: "c1".into(),
				vector: old.clone().into(),
				..Default::default()
			},
			Entity {
				id: "c2".into(),
				vector: old.clone().into(),
				..Default::default()
			},
		];
		store.cold_put_all(&seed).unwrap();

		let client = crate::llm::Client::new_embed_only(&url, "m", "");
		let err = reembed_cold(Some(store.clone()), &client)
			.await
			.expect_err("a mismatched cold batch must surface a partial-failure error");

		assert!(
			err.contains("2 of 2"),
			"names the stale entity count: {err}"
		);
		assert!(
			err.contains("left unchanged"),
			"states the cold tier is untouched: {err}"
		);

		let after = store.cold_all().unwrap();
		assert_eq!(after.len(), 2);
		assert!(
			after.iter().all(|e| e.vector[..] == old[..]),
			"no partial write on failure"
		);

		server.abort();
	}
}
