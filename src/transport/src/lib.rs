//! transport — the portable wire layer: JSON-RPC over tcp/unix/stdio/http/sse/
//! ws/udp, typed request/response channels, the MCP envelope, HTTP server glue,
//! and the kern/hub RPC DTOs + `service!`-generated client/server pairs.
//!
//! Copied byte-for-byte into every project that speaks JSON-RPC. Depends on no
//! kern crate — only `transport-macros` and external crates.
//!
//! Layer: L4 · May import: nothing in kern.

extern crate self as transport;

pub mod http;
pub mod hub_rpc;
pub mod kern_rpc;
pub mod mcp;
pub mod typed;
pub mod wire;

pub use http::serve_http;
pub use mcp::{
	dispatch, serve_rw, serve_stdio, serve_transport, AsDispatch, McpError, McpServer, ToolResult,
	ToolSchema,
};
pub use wire::{select, serve, Dispatch, Sink, Transport};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub use transport_macros::service;

// Re-exports solely for `service!`-generated code (`::transport::__private::*`).
// NOT public API — may change in any release; never import directly.
#[doc(hidden)]
pub mod __private {
	pub use bytes;
	pub use futures;
	pub use serde_json;
	pub use tokio;
	pub use tokio_util;
}
