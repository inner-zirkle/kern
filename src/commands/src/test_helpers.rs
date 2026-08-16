//! Shared test fixtures that need the full kern stack: a daemon core wired to
//! a stub embedder, a scratch daemon endpoint, and a second-writer commit
//! helper.

// A dead port: nothing in the default rig should reach an embedder.
pub(crate) fn rpc_server() -> rpc::server::Server {
	rpc::test_helpers::server()
}

#[cfg(unix)]
pub(crate) fn scratch_endpoint(tag: &str) -> transport::typed::Endpoint {
	// A socket path must fit `sun_path` (`SUN_LEN` 104 on macOS); `now_ms` was
	// 13 digits of that budget for no real gain — pid disambiguates processes
	// and tag disambiguates tests within one, so the pair is unique, and a
	// stale socket from a recycled pid is reclaimed by `bind_unix`.
	let dir = std::env::temp_dir().join(format!("kr-route-{}-{tag}", std::process::id()));
	std::fs::create_dir_all(&dir).expect("scratch dir");
	transport::typed::Endpoint::Unix(dir.join("kern.sock"))
}

#[cfg(unix)]
pub(crate) async fn serving(srv: rpc::server::Server, endpoint: &transport::typed::Endpoint) {
	use std::sync::Arc;
	use transport::typed::{bind_kern_listener, BindOutcome};

	let BindOutcome::Bound(listener) = bind_kern_listener(endpoint).await.expect("bind") else {
		panic!("scratch endpoint already bound");
	};
	let handler = rpc::KernRpcHandler::new(Arc::new(srv), Arc::new(tokio::sync::Notify::new()));
	tokio::spawn(rpc::serve_kern_rpc_loop(listener, handler));
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
			&gg.replica_id,
			gg.quant_mode,
			&std::collections::HashSet::new(),
		)
		.expect("external commit through the shared store");
}
