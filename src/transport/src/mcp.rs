// The MCP envelope over the master wire: the JSON-RPC 2.0 method routing
// (initialize, tools/list, tools/call, shutdown), the `McpServer` a surface
// fills, and the bridge that serves it over any framing in `wire`. Zero-dep
// past serde_json — no serde derive, no thiserror; the two carrier structs
// serialise by hand. This file is copied byte-for-byte into every project's
// `transport` so each is self-contained.

use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::sync::Arc;

use crate::wire::{serve, Dispatch, Sink, Transport};

pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// A tool's advertised shape. Serialised by hand into the MCP wire form.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
	pub name: String,
	pub description: Option<String>,
	pub input_schema: Option<Value>,
}

impl ToolSchema {
	/// Parse one from an MCP tool-definition object; `None` when `name` is
	/// missing. The inverse of [`ToolSchema::to_value`], kept manual so this crate
	/// stays free of a serde-derive dependency.
	pub fn from_value(v: &Value) -> Option<Self> {
		Some(Self {
			name: v.get("name")?.as_str()?.to_string(),
			description: v
				.get("description")
				.and_then(Value::as_str)
				.map(str::to_string),
			input_schema: v.get("inputSchema").cloned(),
		})
	}

	pub fn to_value(&self) -> Value {
		let mut map = serde_json::Map::new();
		map.insert("name".into(), json!(self.name));
		if let Some(d) = &self.description {
			map.insert("description".into(), json!(d));
		}
		if let Some(s) = &self.input_schema {
			map.insert("inputSchema".into(), s.clone());
		}
		Value::Object(map)
	}
}

/// A tool call's result. Serialised by hand into the MCP wire form.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
	pub content: Vec<Value>,
	pub is_error: bool,
	pub structured_content: Option<Value>,
}

impl ToolResult {
	fn to_value(&self) -> Value {
		let mut map = serde_json::Map::new();
		map.insert("content".into(), Value::Array(self.content.clone()));
		map.insert("isError".into(), json!(self.is_error));
		if let Some(s) = &self.structured_content {
			map.insert("structuredContent".into(), s.clone());
		}
		Value::Object(map)
	}
}

/// The failures a served surface reports. `Rpc` carries a JSON-RPC code the
/// caller chose; the rest name transport-level faults a proxy or child raises.
#[derive(Debug)]
pub enum McpError {
	Io(io::Error),
	Protocol(String),
	Json(serde_json::Error),
	Rpc { code: i64, message: String },
	UnknownServer(String),
	DuplicateServer(String),
	NotRunning,
}

impl McpError {
	/// A connection-level fault worth retrying, versus a deterministic one.
	pub fn is_transient(&self) -> bool {
		matches!(self, McpError::Io(_) | McpError::NotRunning)
	}
}

impl std::fmt::Display for McpError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			McpError::Io(e) => write!(f, "mcp transport i/o: {e}"),
			McpError::Protocol(m) => write!(f, "mcp protocol: {m}"),
			McpError::Json(e) => write!(f, "mcp json: {e}"),
			McpError::Rpc { code, message } => write!(f, "mcp rpc error {code}: {message}"),
			McpError::UnknownServer(s) => write!(f, "unknown mcp server: {s}"),
			McpError::DuplicateServer(s) => write!(f, "mcp server already registered: {s}"),
			McpError::NotRunning => write!(f, "mcp child process not running"),
		}
	}
}

impl std::error::Error for McpError {}

impl From<io::Error> for McpError {
	fn from(e: io::Error) -> Self {
		McpError::Io(e)
	}
}

impl From<serde_json::Error> for McpError {
	fn from(e: serde_json::Error) -> Self {
		McpError::Json(e)
	}
}

/// The surface a caller fills once and serves over any transport. Method routing
/// is [`dispatch`]'s; this trait is just the tool vocabulary and the two
/// optional extension points.
pub trait McpServer: Send {
	fn server_name(&self) -> &str {
		"inproc"
	}
	fn server_version(&self) -> &str {
		env!("CARGO_PKG_VERSION")
	}
	fn tools_list(&self) -> Vec<ToolSchema>;
	fn call_tool(&self, name: &str, args: &Value) -> Result<ToolResult, McpError>;
	fn extra_capabilities(&self) -> Value {
		Value::Object(serde_json::Map::new())
	}
	fn handle_method(&self, _method: &str, _params: Value) -> Option<Result<Value, McpError>> {
		None
	}
}

/// Serve an `McpServer` over stdin/stdout — the MCP-child framing a parent
/// spawns.
pub fn serve_stdio(server: &impl McpServer) -> io::Result<i32> {
	let stdin = io::stdin();
	let stdout = io::stdout();
	let mut reader = BufReader::new(stdin.lock());
	let mut writer = stdout.lock();
	serve_rw(&mut reader, &mut writer, server)
}

/// The stdio loop over any reader/writer, so a test can drive it without a
/// process. One frame per line, `shutdown` ends the loop.
pub fn serve_rw<R, W>(reader: &mut R, writer: &mut W, server: &impl McpServer) -> io::Result<i32>
where
	R: BufRead,
	W: Write,
{
	let mut line = String::new();
	loop {
		line.clear();
		let n = reader.read_line(&mut line)?;
		if n == 0 {
			return Ok(0);
		}
		let trimmed = line.trim_end_matches(['\r', '\n']);
		if trimmed.is_empty() {
			continue;
		}
		let frame: Value = match serde_json::from_str(trimmed) {
			Ok(v) => v,
			Err(e) => {
				write_frame(
					writer,
					&error_response(Value::Null, -32700, &format!("parse error: {e}")),
				)?;
				continue;
			}
		};
		let is_shutdown = frame.get("method").and_then(Value::as_str) == Some("shutdown");
		if let Some(response) = dispatch(server, &frame) {
			write_frame(writer, &response)?;
		}
		if is_shutdown {
			return Ok(0);
		}
	}
}

/// Route one JSON-RPC frame through an `McpServer`, returning its response frame
/// or `None` for a notification. The one place the MCP protocol is spelled, so
/// every transport answers a frame identically.
pub fn dispatch(server: &dyn McpServer, frame: &Value) -> Option<Value> {
	let id = frame.get("id").cloned();
	let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
	let params = frame.get("params").cloned().unwrap_or(Value::Null);
	let is_notification = id.is_none() || id.as_ref() == Some(&Value::Null);

	match method {
		"initialize" => {
			let mut caps = serde_json::Map::new();
			caps.insert("tools".to_string(), json!({}));
			if let Value::Object(extra) = server.extra_capabilities() {
				caps.extend(extra);
			}
			let reply = json!({
				"protocolVersion": PROTOCOL_VERSION,
				"capabilities": caps,
				"serverInfo": {
					"name": server.server_name(),
					"version": server.server_version(),
				},
			});
			id.map(|id| ok_response(id, reply))
		}
		"notifications/initialized" => None,
		"tools/list" => {
			if is_notification {
				return None;
			}
			let tools: Vec<Value> = server
				.tools_list()
				.iter()
				.map(ToolSchema::to_value)
				.collect();
			id.map(|id| ok_response(id, json!({ "tools": tools })))
		}
		"tools/call" => {
			if is_notification {
				return None;
			}
			let name = params.get("name").and_then(Value::as_str).unwrap_or("");
			let args = params.get("arguments").cloned().unwrap_or(Value::Null);
			let result = server.call_tool(name, &args).map(|r| r.to_value());
			id.map(|id| match result {
				Ok(v) => ok_response(id, v),
				Err(e) => {
					let (code, message) = rpc_code_message(e);
					error_response(id, code, &message)
				}
			})
		}
		"shutdown" => id.map(|id| ok_response(id, Value::Null)),
		_ => {
			if is_notification {
				return None;
			}
			match server.handle_method(method, params) {
				Some(Ok(v)) => id.map(|id| ok_response(id, v)),
				Some(Err(e)) => id.map(|id| {
					let (code, msg) = rpc_code_message(e);
					error_response(id, code, &msg)
				}),
				None => id.map(|id| error_response(id, -32601, &format!("method not found: {method}"))),
			}
		}
	}
}

fn rpc_code_message(e: McpError) -> (i64, String) {
	match e {
		McpError::Rpc { code, message } => (code, message),
		other => (-32000, other.to_string()),
	}
}

pub fn ok_response(id: Value, result: Value) -> Value {
	json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
	json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn write_frame<W: Write>(w: &mut W, value: &Value) -> io::Result<()> {
	let mut line =
		serde_json::to_string(value).map_err(|e| io::Error::other(format!("serialise frame: {e}")))?;
	if line.contains('\n') {
		return Err(io::Error::other("frame contained newline"));
	}
	line.push('\n');
	w.write_all(line.as_bytes())?;
	w.flush()?;
	Ok(())
}

// --- bridge: McpServer -> Dispatch, served over any wire framing ---

/// Wraps a shared `McpServer` as a [`Dispatch`]. MCP methods route through the
/// same [`dispatch`] the stdio path uses, so a frame answered over ws or tcp is
/// byte-for-byte the one stdio would answer.
pub struct AsDispatch<S>(pub Arc<S>);

impl<S: McpServer + Send + Sync> Dispatch for AsDispatch<S> {
	fn call(&self, frame: &Value, _sink: &mut Sink) -> Option<Value> {
		dispatch(self.0.as_ref(), frame)
	}
}

/// Serve a shared `McpServer` over one wire framing until the process ends.
pub fn serve_transport<S: McpServer + Send + Sync + 'static>(
	transport: Transport,
	server: Arc<S>,
) -> Result<(), String> {
	serve(transport, Arc::new(AsDispatch(server)))
}
