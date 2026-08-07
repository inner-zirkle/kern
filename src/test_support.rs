//! Shared test fixtures: deterministic embeddings, canned LLM closures, and
//! graph builders — test-only, compiled into the lib for reuse by integration
//! tests.

use crate::base_types::{Entity, Reason};
use tokio::task::JoinHandle;

// Lets a test assert on the allocation a call makes. A buffer removal cannot be
// checked by the result, which is unchanged by construction, nor by the clock,
// which is noise on a loaded box — only by watching the bytes go out.
pub(crate) mod alloc_probe {
	use std::alloc::{GlobalAlloc, Layout, System};
	use std::cell::Cell;

	thread_local! {
		static BYTES: Cell<usize> = const { Cell::new(0) };
		static PEAK: Cell<usize> = const { Cell::new(0) };
	}

	// `added` is what the heap grew by, `block` the size of the region handed back —
	// they differ on `realloc`, where the caller ends up holding `block` bytes but
	// only paid for the difference.
	fn record(added: usize, block: usize) {
		let _ = BYTES.try_with(|b| b.set(b.get() + added));
		let _ = PEAK.try_with(|p| p.set(p.get().max(block)));
	}

	pub(crate) struct Counting;

	// Every method forwards to `System` untouched. The counters are const-init
	// `Cell`s with no destructor, so touching them allocates nothing and cannot
	// re-enter the allocator.
	unsafe impl GlobalAlloc for Counting {
		unsafe fn alloc(&self, l: Layout) -> *mut u8 {
			record(l.size(), l.size());
			unsafe { System.alloc(l) }
		}
		unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
			record(l.size(), l.size());
			unsafe { System.alloc_zeroed(l) }
		}
		unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
			unsafe { System.dealloc(p, l) }
		}
		unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
			record(new.saturating_sub(l.size()), new);
			unsafe { System.realloc(p, l, new) }
		}
	}

	#[derive(Debug)]
	pub(crate) struct Allocated {
		pub total: usize,
		// The largest single block. An N-wide buffer shows up here and nowhere
		// else, so this is what separates it from the top-k tail.
		pub peak: usize,
	}

	// Bytes handed to the CALLING thread while `f` ran.
	pub(crate) fn measure<R>(f: impl FnOnce() -> R) -> (R, Allocated) {
		let before = BYTES.with(|b| b.get());
		PEAK.with(|p| p.set(0));
		let r = f();
		let a = Allocated {
			total: BYTES.with(|b| b.get()) - before,
			peak: PEAK.with(|p| p.get()),
		};
		(r, a)
	}
}

#[global_allocator]
static COUNTING: alloc_probe::Counting = alloc_probe::Counting;

pub(crate) fn entity(id: &str) -> Entity {
	Entity {
		id: id.into(),
		..Default::default()
	}
}

pub(crate) fn entity_vec(id: &str, vector: Vec<f32>) -> Entity {
	Entity {
		id: id.into(),
		vector: vector.into(),
		..Default::default()
	}
}

pub(crate) fn edge(from: &str, to: &str) -> Reason {
	Reason {
		id: format!("{from}->{to}"),
		from: from.into(),
		to: to.into(),
		..Default::default()
	}
}

// A dead port: nothing in the default rig should reach an embedder.
pub(crate) fn mcp_server() -> crate::mcp::Server {
	mcp_server_with_embed_url("http://127.0.0.1:1")
}

// Same server against a live stub embedder, for tests that have to follow an
// ingest all the way into the graph rather than stop at the tool boundary.
pub(crate) fn mcp_server_with_embed_url(url: &str) -> crate::mcp::Server {
	use parking_lot::RwLock;
	use std::sync::Arc;
	let graph = Arc::new(RwLock::new(crate::graph::GraphGnn::new()));
	let embedder = crate::llm::Client::new_embed_only(url, "test", "");
	let worker = Arc::new(crate::ingest::Worker::new(
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
		cfg: Arc::new(crate::config::Config::default()),
		broadcast_pulse: None,
		last_activity: Arc::new(std::sync::atomic::AtomicU64::new(crate::util::now_ms())),
	}
}

// The default rig with a caller-shaped config — for tools that resolve paths
// (peer key, intake dir) off cfg rather than the graph.
pub(crate) fn mcp_server_with_config(cfg: crate::config::Config) -> crate::mcp::Server {
	let mut s = mcp_server();
	s.cfg = std::sync::Arc::new(cfg);
	s
}

#[cfg(unix)]
pub(crate) fn scratch_endpoint(tag: &str) -> crate::transport::typed::Endpoint {
	let dir = std::env::temp_dir().join(format!(
		"kern-route-{}-{}-{tag}",
		std::process::id(),
		crate::util::now_ms()
	));
	std::fs::create_dir_all(&dir).expect("scratch dir");
	crate::transport::typed::Endpoint::Unix(dir.join("kern.sock"))
}

// The secret a scratch daemon demands, and the identity its scratch clients
// present. A fixed pair, because these endpoints live in temp dirs with no
// config and no `mcp-token` to read.
#[cfg(unix)]
pub(crate) const TEST_TOKEN: &str = "scratch-token";

#[cfg(unix)]
pub(crate) fn test_caller() -> crate::transport::kern_rpc::AuthReq {
	crate::transport::kern_rpc::AuthReq::new(TEST_TOKEN)
}

#[cfg(unix)]
pub(crate) async fn serving(srv: crate::mcp::Server, endpoint: &crate::transport::typed::Endpoint) {
	use crate::transport::typed::{bind_kern_listener, BindOutcome};
	use std::sync::Arc;

	let BindOutcome::Bound(listener) = bind_kern_listener(endpoint).await.expect("bind") else {
		panic!("scratch endpoint already bound");
	};
	let handler =
		crate::rpc::KernRpcHandler::new(Arc::new(srv), Arc::new(tokio::sync::Notify::new()));
	tokio::spawn(crate::rpc::serve_kern_rpc_loop(
		listener,
		handler,
		TEST_TOKEN.to_string(),
	));
}

pub(crate) fn tool_text(v: &serde_json::Value) -> String {
	v["content"][0]["text"].as_str().unwrap_or("").to_string()
}

// An embed endpoint that never answers. Pins the ingest worker on one job so a
// test can fill the queue behind it.
pub(crate) fn hanging_embed_app() -> axum::Router {
	axum::Router::new().route(
		"/api/embed",
		axum::routing::post(|_b: axum::Json<serde_json::Value>| async move {
			std::future::pending::<axum::Json<serde_json::Value>>().await
		}),
	)
}

// Every text embeds to the same vector, so a test can drive the whole ingest
// path — document plus chunks — without a live model. Answers one embedding per
// `input` entry; a fixed-length reply starves `embed_chunks`.
pub(crate) fn fixed_vec_embed_app() -> axum::Router {
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

pub(crate) async fn spawn_http(app: axum::Router) -> (String, JoinHandle<()>) {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = listener.local_addr().unwrap();
	let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
	(format!("http://{addr}"), handle)
}

// A second writer committing straight through the shared store — how a daemon
// advances the epoch underneath a one-shot CLI command mid-flight.
pub(crate) fn commit_extra_kern_via_store(
	g: &std::sync::Arc<parking_lot::RwLock<crate::graph::GraphGnn>>,
	kern: crate::base_types::Kern,
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
