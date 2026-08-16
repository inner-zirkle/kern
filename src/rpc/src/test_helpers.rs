//! Test helpers used by `rpc`'s own unit tests AND by external crate tests
//! (e.g. `commands`) that need a `Server` wired to a stub embedder.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::server::Server;

/// A dead port: nothing in the default rig should reach an embedder.
pub fn server() -> Server {
	server_with_embed_url("http://127.0.0.1:1")
}

/// Same server against a live stub embedder, for tests that have to follow an
/// ingest all the way into the graph rather than stop at the operation boundary.
pub fn server_with_embed_url(url: &str) -> Server {
	let graph = Arc::new(RwLock::new(graph::graph::GraphGnn::new()));
	let embedder = llm::Client::new_embed_only(url, "test", "");
	let worker = Arc::new(ingest::Worker::new(
		graph.clone(),
		embedder,
		None,
		None,
		None,
	));
	Server {
		graph,
		worker,
		llm: None,
		save_fn: Arc::new(|| {}),
		task_q: None,
		cfg: Arc::new(config::Config::default()),
		broadcast_pulse: None,
		last_activity: Arc::new(std::sync::atomic::AtomicU64::new(util::now_ms())),
		query_cache: crate::server::QueryCache::default(),
	}
}

/// The default rig with a caller-shaped config — for operations that resolve
/// paths (peer key, intake dir) off cfg rather than the graph.
pub fn server_with_config(cfg: config::Config) -> Server {
	let mut s = server();
	s.cfg = std::sync::Arc::new(cfg);
	s
}
