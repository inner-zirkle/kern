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
pub(crate) fn mcp_server() -> mcp::Server {
	mcp::test_helpers::mcp_server()
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
pub(crate) async fn serving(srv: mcp::Server, endpoint: &transport::typed::Endpoint) {
	use std::sync::Arc;
	use transport::typed::{bind_kern_listener, BindOutcome};

	let BindOutcome::Bound(listener) = bind_kern_listener(endpoint).await.expect("bind") else {
		panic!("scratch endpoint already bound");
	};
	let handler = rpc::KernRpcHandler::new(Arc::new(srv), Arc::new(tokio::sync::Notify::new()));
	tokio::spawn(rpc::serve_kern_rpc_loop(
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
