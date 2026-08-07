//! Wire servers: one [`Dispatch`] trait served over tcp/unix/stdio/http/sse/
//! ws/udp, so a surface picks transports by listing them.

// One transport family for every surface in the system. kern, ctrl and the
// agent each speak JSON-RPC 2.0 already — kern/ctrl as MCP tools, the agent as
// a single "chat" method — so the wire that carries a JSON-RPC frame is the wire
// they can all share. This crate is that wire: nine framings of one contract,
// zero dependencies past serde_json, parallel by std::thread.
//
// The contract is `Dispatch`: a JSON-RPC request frame in, an optional response
// frame out, tokens streamed to a sink as they are produced. A surface implements
// it once and gains every protocol; a caller picks the protocol and the method,
// never a bespoke endpoint.
//
//   Uniform transport, per-surface methods. The framing is universal; the method
//   vocabulary (`tools/call` for kern/ctrl, `chat` for the agent) stays each
//   surface's own.

use serde_json::Value;
use std::sync::Arc;

mod http;
mod sse;
mod stdio;
mod tcp;
mod udp;
mod unix;
mod ws;

/// Streamed content tokens. A transport that can stream (ws, sse, stdio-tty)
/// feeds each token here as it arrives; one that cannot (http, udp) passes a
/// no-op. The final frame from [`Dispatch::call`] is always sent regardless.
pub type Sink<'a> = dyn FnMut(&str) + 'a;

/// The one contract every served surface fills. `frame` is a parsed JSON-RPC
/// 2.0 request; the return is its response frame, or `None` for a notification
/// (a request without an `id`), which is answered with silence.
///
/// `Send + Sync` because the wire is thread-per-connection: one dispatcher is
/// shared across every connection behind an `Arc`. A surface with interior
/// state guards it (kern is already `Arc`-shared; ctrl moves off `RefCell`).
pub trait Dispatch: Send + Sync {
	fn call(&self, frame: &Value, sink: &mut Sink) -> Option<Value>;
}

/// The protocol a surface is served over. Every variant carries the same
/// JSON-RPC frame; they differ only in how a frame is delimited on the wire.
pub enum Transport {
	/// Newline-delimited JSON-RPC over stdin/stdout — MCP's own framing, how a
	/// parent spawns this surface as a child.
	Stdio,
	/// Newline-delimited JSON-RPC over TCP, one frame per line.
	Tcp(u16),
	/// Newline-delimited JSON-RPC over a Unix domain socket — local IPC, no
	/// network exposure.
	Unix(String),
	/// Datagram UDP: one datagram a frame, one datagram back. No streaming.
	Udp(u16),
	/// HTTP/1.1: POST a frame, receive its response frame. No streaming.
	Http(u16),
	/// Server-sent events: POST a frame, the answer streams back as `data:`
	/// events — one per token, then the response frame.
	Sse(u16),
	/// WebSocket (RFC 6455): a text frame is a request; the answer streams back
	/// as `{"token":...}` frames closed by the response frame.
	Ws(u16),
}

/// Serve `dispatch` over `transport` until the process ends. Blocking. Each
/// connection is handled on its own thread, so calls run in parallel; the
/// dispatcher is cloned by `Arc` into every thread.
pub fn serve(transport: Transport, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	match transport {
		Transport::Stdio => stdio::serve(dispatch),
		Transport::Tcp(port) => tcp::serve(port, dispatch),
		Transport::Unix(path) => unix::serve(&path, dispatch),
		Transport::Udp(port) => udp::serve(port, dispatch),
		Transport::Http(port) => http::serve(port, dispatch),
		Transport::Sse(port) => sse::serve(port, dispatch),
		Transport::Ws(port) => ws::serve(port, dispatch),
	}
}

/// Pick the transport an argument list names, the way each binary's `main`
/// selects one: `--tcp/--http/--ws <port>`, `--unix <path>`, or stdio by
/// default (`--stdio`, or no flag at all).
pub fn select(args: &[String]) -> Result<Transport, String> {
	let mut it = args.iter().peekable();
	while let Some(flag) = it.next() {
		match flag.as_str() {
			"--stdio" | "--mcp" => return Ok(Transport::Stdio),
			"--unix" => {
				let path = it.next().ok_or("--unix needs a socket path")?;
				return Ok(Transport::Unix(path.clone()));
			}
			"--tcp" | "--udp" | "--http" | "--sse" | "--ws" => {
				let raw = it.next().ok_or_else(|| format!("{flag} needs a port"))?;
				let port = raw
					.parse::<u16>()
					.map_err(|_| format!("{flag} needs a port, got '{raw}'"))?;
				return Ok(match flag.as_str() {
					"--tcp" => Transport::Tcp(port),
					"--udp" => Transport::Udp(port),
					"--http" => Transport::Http(port),
					"--sse" => Transport::Sse(port),
					_ => Transport::Ws(port),
				});
			}
			_ => continue,
		}
	}
	Ok(Transport::Stdio)
}

/// A JSON-RPC 2.0 error response frame, shared by every transport so a malformed
/// or unroutable frame answers the same on every wire.
pub(crate) fn error(id: Value, code: i64, message: &str) -> Value {
	serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Parse one line into a frame and dispatch it, returning the response frame to
/// write (or `None` for a notification). The line framing shared by stdio, tcp
/// and unix; `-32700` for an unparseable line.
pub(crate) fn line_frame(line: &str, dispatch: &dyn Dispatch, sink: &mut Sink) -> Option<Value> {
	match serde_json::from_str::<Value>(line) {
		Ok(frame) => dispatch.call(&frame, sink),
		Err(_) => Some(error(Value::Null, -32700, "parse error")),
	}
}
