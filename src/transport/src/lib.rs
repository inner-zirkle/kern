mod http;
// The portable transport layer, copied byte-for-byte into every project:
//   wire  — seven framings of one JSON-RPC contract
//   mcp   — the MCP envelope (McpServer, dispatch, serve, bridge)
// kern adds its own federation on top (http, typed, kern_rpc, hub_rpc).
pub mod mcp;
pub mod wire;

// Root re-exports keep `transport::McpServer`, `transport::serve_stdio`, etc
// working unchanged — the mcp envelope moved into one module, the surface did not.
pub use http::serve_http;
pub use mcp::{
	dispatch, serve_rw, serve_stdio, serve_transport, AsDispatch, McpError, McpServer, ToolResult,
	ToolSchema,
};
pub use wire::{select, serve, Dispatch, Sink, Transport};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

// `service!` emits `::transport::*` paths; the self-alias makes them resolve
// when the macro is invoked inside this crate.
extern crate self as transport;

pub mod typed;
pub use transport_macros::service;

pub mod hub_rpc;
pub mod kern_rpc;

// Re-exports solely for service!-generated code (::transport::__private::*).
// NOT public API — may change in any release; never import directly.
#[doc(hidden)]
pub mod __private {
	pub use bytes;
	pub use futures;
	pub use serde_json;
	pub use tokio;
	pub use tokio_util;
}
