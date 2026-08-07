//! test_support — shared test fixtures that depend only on the leaf crates.
//!
//! Deterministic entity/edge builders, canned HTTP embed stubs, and the
//! counting allocator (`alloc_probe`) a test installs as its
//! `#[global_allocator]` to measure bytes a call allocates. Nothing here may
//! import a crate above `base` — every other crate pulls this in as a
//! *dev*-dependency, and a dev-dependency that reached back up would cycle.
//!
//! Test-only. Never a normal dependency.

pub mod alloc_probe;

pub fn entity(id: &str) -> base::base_types::Entity {
	base::base_types::Entity {
		id: id.into(),
		..Default::default()
	}
}

pub fn entity_vec(id: &str, vector: Vec<f32>) -> base::base_types::Entity {
	base::base_types::Entity {
		id: id.into(),
		vector: vector.into(),
		..Default::default()
	}
}

pub fn edge(from: &str, to: &str) -> base::base_types::Reason {
	base::base_types::Reason {
		id: format!("{from}->{to}"),
		from: from.into(),
		to: to.into(),
		..Default::default()
	}
}

pub fn tool_text(v: &serde_json::Value) -> String {
	v["content"][0]["text"].as_str().unwrap_or("").to_string()
}

/// An embed endpoint that never answers. Pins an ingest worker on one job so a
/// test can fill the queue behind it.
pub fn hanging_embed_app() -> axum::Router {
	axum::Router::new().route(
		"/api/embed",
		axum::routing::post(|_b: axum::Json<serde_json::Value>| async move {
			std::future::pending::<axum::Json<serde_json::Value>>().await
		}),
	)
}

/// Every text embeds to the same vector, so a test can drive the whole ingest
/// path without a live model. Answers one embedding per `input` entry.
pub fn fixed_vec_embed_app() -> axum::Router {
	axum::Router::new().route(
		"/api/embed",
		axum::routing::post(|body: axum::Json<serde_json::Value>| async move {
			let n = body
				.0
				.get("input")
				.and_then(|v| v.as_array())
				.map(|a| a.len())
				.unwrap_or(1);
			let embs: Vec<Vec<f32>> = (0..n).map(|_| vec![0.1, 0.2, 0.3]).collect();
			axum::Json(serde_json::json!({ "embeddings": embs }))
		}),
	)
}

pub async fn spawn_http(app: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	(format!("http://{addr}"), handle)
}
