//! Test-only helpers. A dead-port MCP server with a stub embedder is the
//! default; tests that need a real embed reach for `mcp_server_with_embed_url`.

#[cfg(test)]
pub mod inner {
    use std::sync::Arc;

    use parking_lot::RwLock;

    /// A dead port: nothing in the default rig should reach an embedder.
    pub fn mcp_server() -> crate::Server {
        mcp_server_with_embed_url("http://127.0.0.1:1")
    }

    /// Same server against a live stub embedder, for tests that have to follow
    /// an ingest all the way into the graph rather than stop at the tool boundary.
    pub fn mcp_server_with_embed_url(url: &str) -> crate::Server {
        let graph = Arc::new(RwLock::new(graph::graph::GraphGnn::new()));
        let embedder = llm::Client::new_embed_only(url, "test", "");
        let worker = Arc::new(ingest::Worker::new(
            graph.clone(),
            embedder,
            None,
            None,
            None,
        ));
        crate::Server {
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

    /// The default rig with a caller-shaped config — for tools that resolve
    /// paths (peer key, intake dir) off cfg rather than the graph.
    pub fn mcp_server_with_config(cfg: config::Config) -> crate::Server {
        let mut s = mcp_server();
        s.cfg = std::sync::Arc::new(cfg);
        s
    }
}
