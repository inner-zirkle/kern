//! MCP over Streamable HTTP (the module name predates the rename): resolve
//! the bearer token from the server's own config and serve through
//! `transport::serve_http` — the surface is never up unauthenticated.

use std::path::Path;
use std::sync::Arc;

use crate::mcp::Server;

// Despite the module name `sse`, this is MCP Streamable HTTP, not a WebSocket.
// The token is resolved from the server's own config (minted on first use), so
// the surface is never served unauthenticated and needs no caller wiring.
pub async fn run_sse(server: Arc<Server>, addr: &str) -> Result<(), std::io::Error> {
	let token = server
		.cfg
		.serve
		.resolve_mcp_token(Path::new(&server.cfg.data_dir))?;
	tracing::info!(
		target: "kern.mcp_sse",
		token_file = %crate::config::mcp_token_path(Path::new(&server.cfg.data_dir)).display(),
		"MCP-over-HTTP requires a bearer token"
	);
	crate::transport::serve_http(server, addr, Some(&token)).await
}
