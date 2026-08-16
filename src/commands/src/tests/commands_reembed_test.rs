//! Tests extracted from commands_reembed.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[tokio::test]
	async fn a_completed_reembed_restamps_the_store_with_the_new_model() {
		use base::base_types::Entity;
		use store_core::{EmbedCheck, EmbedStamp, Store};

		// Fake embed endpoint: one 2-dim vector per input, any batch size.
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|body: axum::Json<serde_json::Value>| async move {
				let n = body["input"].as_array().map_or(1, |a| a.len());
				let vecs: Vec<Vec<f64>> = (0..n).map(|_| vec![0.1, 0.2]).collect();
				axum::Json(serde_json::json!({ "embeddings": vecs }))
			}),
		);
		let (url, server) = test_support::spawn_http(app).await;

		let dir = tempfile::tempdir().unwrap();
		let mut cfg = config::Config::default_in(dir.path());
		cfg.embed.model = "new-model".into();

		// A store holding one 3-dim entity, stamped with the model that made it.
		{
			let store = std::sync::Arc::new(Store::open(&cfg.data_dir).unwrap());
			let mut g = graph::graph::GraphGnn::new();
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
			graph::persist::save_graph_into(&store, &g).unwrap();
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
		let (url, server) = test_support::spawn_http(app).await;

		let client = llm::Client::new_embed_only(&url, "test-model", "");
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
		use base::base_types::Entity;
		use store_core::Store;

		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|| async {
				axum::Json(serde_json::json!({ "embeddings": [[0.5, 0.5]] }))
			}),
		);
		let (url, server) = test_support::spawn_http(app).await;

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

		let client = llm::Client::new_embed_only(&url, "m", "");
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
