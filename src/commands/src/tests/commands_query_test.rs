//! Tests extracted from commands_query.rs
#![allow(unused)]
use super::*;

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
