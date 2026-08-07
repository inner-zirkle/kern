//! Shared test fixtures that need the full kern stack: an MCP server wired to
//! a stub embedder, a scratch daemon endpoint, and a second-writer commit
//! helper. The leaf fixtures (entity/edge/embed stubs/alloc_probe) live in the
//! `test_support` crate and are re-exported here so existing `crate::test_support`
//! call sites keep working.

#[allow(unused_imports)]
pub(crate) use test_support::{
	alloc_probe, edge, entity, hanging_embed_app, spawn_http, tool_text,
};

// A dead port: nothing in the default rig should reach an embedder.
pub(crate) fn mcp_server() -> crate::mcp::Server {
	mcp_server_with_embed_url("http://127.0.0.1:1")
}

// Same server against a live stub embedder, for tests that have to follow an
// ingest all the way into the graph rather than stop at the tool boundary.
pub(crate) fn mcp_server_with_embed_url(url: &str) -> crate::mcp::Server {
	use parking_lot::RwLock;
	use std::sync::Arc;
	let graph = Arc::new(RwLock::new(graph::graph::GraphGnn::new()));
	let embedder = llm::Client::new_embed_only(url, "test", "");
	let worker = Arc::new(ingest::Worker::new(
		graph.clone(),
		embedder,
		None,
		None,
		None,
	));
	crate::mcp::Server {
		graph,
		worker,
		llm: None,
		save_fn: Arc::new(|| {}),
		task_q: None,
		cfg: Arc::new(config::Config::default()),
		broadcast_pulse: None,
		last_activity: Arc::new(std::sync::atomic::AtomicU64::new(util::now_ms())),
	}
}

// The default rig with a caller-shaped config — for tools that resolve paths
// (peer key, intake dir) off cfg rather than the graph.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn mcp_server_with_config(cfg: config::Config) -> crate::mcp::Server {
	let mut s = mcp_server();
	s.cfg = std::sync::Arc::new(cfg);
	s
}

#[cfg(unix)]
pub(crate) fn scratch_endpoint(tag: &str) -> transport::typed::Endpoint {
	let dir = std::env::temp_dir().join(format!(
		"kern-route-{}-{}-{tag}",
		std::process::id(),
		util::now_ms()
	));
	std::fs::create_dir_all(&dir).expect("scratch dir");
	transport::typed::Endpoint::Unix(dir.join("kern.sock"))
}

#[cfg(unix)]
pub(crate) const TEST_TOKEN: &str = "scratch-token";

#[cfg(unix)]
pub(crate) fn test_caller() -> transport::kern_rpc::AuthReq {
	transport::kern_rpc::AuthReq::new(TEST_TOKEN)
}

#[cfg(unix)]
pub(crate) async fn serving(srv: crate::mcp::Server, endpoint: &transport::typed::Endpoint) {
	use std::sync::Arc;
	use transport::typed::{bind_kern_listener, BindOutcome};

	let BindOutcome::Bound(listener) = bind_kern_listener(endpoint).await.expect("bind") else {
		panic!("scratch endpoint already bound");
	};
	let handler =
		::rpc::KernRpcHandler::new(Arc::new(srv), Arc::new(tokio::sync::Notify::new()));
	tokio::spawn(::rpc::serve_kern_rpc_loop(
		listener,
		handler,
		TEST_TOKEN.to_string(),
	));
}

// A second writer committing straight through the shared store — how a daemon
// advances the epoch underneath a one-shot CLI command mid-flight.
pub(crate) fn commit_extra_kern_via_store(
	g: &std::sync::Arc<parking_lot::RwLock<graph::graph::GraphGnn>>,
	kern: base::base_types::Kern,
) {
	let gg = g.read();
	let store = gg.store().expect("graph has a bound store");
	let mut kerns = std::collections::HashMap::new();
	for k in gg.all() {
		kerns.insert(k.id.clone(), k.clone());
	}
	kerns.insert(gg.root.id.clone(), gg.root.clone());
	kerns.insert(kern.id.clone(), kern);
	store
		.save_all_kerns(
			&kerns,
			&gg.network_id,
			gg.quant_mode,
			&std::collections::HashSet::new(),
		)
		.expect("external commit through the shared store");
}
