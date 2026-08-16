//! transport — the local-RPC substrate: typed request/response channels over
//! a Unix socket or Windows named pipe, plus the kern/hub RPC DTOs +
//! `service!`-generated client/server pairs.
//!
//! Depends on no kern crate — only `transport-macros` and external crates.
//!
//! Layer: L4 · May import: nothing in kern.

extern crate self as transport;

pub mod hub_rpc;
pub mod kern_rpc;
pub mod typed;

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
