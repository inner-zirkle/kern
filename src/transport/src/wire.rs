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
		Transport::Stdio => serve_stdio(dispatch),
		Transport::Tcp(port) => serve_tcp(port, dispatch),
		Transport::Unix(path) => serve_unix(&path, dispatch),
		Transport::Udp(port) => serve_udp(port, dispatch),
		Transport::Http(port) => serve_http(port, dispatch),
		Transport::Sse(port) => serve_sse(port, dispatch),
		Transport::Ws(port) => serve_ws(port, dispatch),
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

// ==== [stdio] ====

// Newline-delimited JSON-RPC over stdin/stdout: MCP's own framing, and how a
// parent process spawns this surface as a child. One frame per line in, one
// response line out; a notification (no id) answers with silence. Serial by
// nature — stdio is a single stream — so no threads here.

use std::io::{self, BufRead, Write};

fn serve_stdio(dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	let stdin = io::stdin();
	let mut stdout = io::stdout();
	for line in stdin.lock().lines() {
		let line = line.map_err(|e| format!("stdio read: {e}"))?;
		let trimmed = line.trim();
		if trimmed.is_empty() {
			continue;
		}
		if let Some(frame) = line_frame(trimmed, dispatch.as_ref(), &mut |_| {}) {
			writeln!(stdout, "{frame}").map_err(|e| format!("stdio write: {e}"))?;
			stdout.flush().map_err(|e| format!("stdio flush: {e}"))?;
		}
	}
	Ok(())
}

// ==== [tcp] ====

// Newline-delimited JSON-RPC over TCP: one frame per line, one response line
// back. Each accepted connection is served on its own thread, so frames from
// different clients dispatch in parallel; the dispatcher is shared by Arc.

use std::io::BufReader;
use std::net::TcpListener;

fn serve_tcp(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("tcp bind: {e}"))?;
	for stream in listener.incoming() {
		let Ok(mut stream) = stream else { continue };
		let dispatch = dispatch.clone();
		std::thread::spawn(move || {
			let Ok(reader) = stream.try_clone() else {
				return;
			};
			for line in BufReader::new(reader).lines() {
				let Ok(line) = line else { break };
				let line = line.trim();
				if line.is_empty() {
					continue;
				}
				if let Some(frame) = line_frame(line, dispatch.as_ref(), &mut |_| {}) {
					if writeln!(stream, "{frame}").is_err() {
						break;
					}
				}
			}
		});
	}
	Ok(())
}

// ==== [udp] ====

// Datagram UDP: one datagram in is one JSON-RPC frame, one datagram back is the
// response frame. No streaming — a datagram has no stream — so tokens are
// dropped and the response frame carries the whole. An answer past 65507 bytes
// (the datagram ceiling) is refused with a -32000 frame rather than truncated,
// since a truncated JSON frame is unparseable. Single socket, served in a loop.

use std::net::UdpSocket;

const MAX_DATAGRAM: usize = 65507;

fn serve_udp(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	let socket = UdpSocket::bind(("0.0.0.0", port)).map_err(|e| format!("udp bind: {e}"))?;
	let mut buffer = [0u8; MAX_DATAGRAM];
	loop {
		let (received, peer) = socket
			.recv_from(&mut buffer)
			.map_err(|e| format!("udp recv: {e}"))?;
		let response = match serde_json::from_slice::<Value>(&buffer[..received]) {
			Ok(frame) => match dispatch.call(&frame, &mut |_| {}) {
				// A notification: nothing to send back.
				None => continue,
				Some(response) => response,
			},
			Err(_) => error(Value::Null, -32700, "parse error"),
		};
		let mut bytes = response.to_string().into_bytes();
		if bytes.len() > MAX_DATAGRAM {
			let id = response.get("id").cloned().unwrap_or(Value::Null);
			bytes = error(id, -32000, "response exceeds datagram limit; use tcp")
				.to_string()
				.into_bytes();
		}
		if let Err(e) = socket.send_to(&bytes, peer) {
			return Err(format!("udp send: {e}"));
		}
	}
}

// ==== [unix] ====

// Newline-delimited JSON-RPC over a Unix domain socket: local IPC, no network
// exposure, same line framing as tcp. A stale socket file from a prior run is
// removed before binding. Thread-per-connection, dispatcher shared by Arc.

use std::os::unix::net::UnixListener;

fn serve_unix(path: &str, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	let _ = std::fs::remove_file(path);
	let listener = UnixListener::bind(path).map_err(|e| format!("unix bind {path}: {e}"))?;
	for stream in listener.incoming() {
		let Ok(mut stream) = stream else { continue };
		let dispatch = dispatch.clone();
		std::thread::spawn(move || {
			let Ok(reader) = stream.try_clone() else {
				return;
			};
			for line in BufReader::new(reader).lines() {
				let Ok(line) = line else { break };
				let line = line.trim();
				if line.is_empty() {
					continue;
				}
				if let Some(frame) = line_frame(line, dispatch.as_ref(), &mut |_| {}) {
					if writeln!(stream, "{frame}").is_err() {
						break;
					}
				}
			}
		});
	}
	Ok(())
}

// ==== [http] ====

// HTTP/1.1 on std::net: POST a JSON-RPC frame, receive its response frame as
// the body. No streaming — one request, one response. Thread-per-connection;
// the dispatcher is shared by Arc. 405 for anything but POST, 400 for a body
// that is not a frame.

use std::io::Read;
use std::net::TcpStream;

fn serve_http(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("http bind: {e}"))?;
	for stream in listener.incoming() {
		let Ok(mut stream) = stream else { continue };
		let dispatch = dispatch.clone();
		std::thread::spawn(move || {
			let _ = respond_http(&mut stream, dispatch.as_ref());
		});
	}
	Ok(())
}

fn respond_http(stream: &mut TcpStream, dispatch: &dyn Dispatch) -> Result<(), String> {
	let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("http: {e}"))?);

	let mut request_line = String::new();
	reader
		.read_line(&mut request_line)
		.map_err(|e| format!("http read: {e}"))?;
	if !request_line.starts_with("POST ") {
		return write_json(
			stream,
			405,
			&error(Value::Null, -32600, "POST a JSON-RPC frame"),
		);
	}

	let mut content_length = 0usize;
	loop {
		let mut header = String::new();
		reader
			.read_line(&mut header)
			.map_err(|e| format!("http read: {e}"))?;
		let header = header.trim();
		if header.is_empty() {
			break;
		}
		if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
			content_length = value.trim().parse().unwrap_or(0);
		}
	}

	let mut body = vec![0u8; content_length];
	reader
		.read_exact(&mut body)
		.map_err(|e| format!("http body: {e}"))?;

	let frame = match serde_json::from_slice::<Value>(&body) {
		Ok(frame) => frame,
		Err(_) => return write_json(stream, 400, &error(Value::Null, -32700, "parse error")),
	};

	// http cannot stream; tokens are dropped, the response frame carries the whole.
	match dispatch.call(&frame, &mut |_| {}) {
		Some(response) => write_json(stream, 200, &response),
		// A notification: 204, no body.
		None => write!(
			stream,
			"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
		)
		.map_err(|e| format!("http write: {e}")),
	}
}

fn write_json(stream: &mut TcpStream, status: u16, body: &Value) -> Result<(), String> {
	let reason = match status {
		200 => "OK",
		400 => "Bad Request",
		405 => "Method Not Allowed",
		_ => "Internal Server Error",
	};
	let body = body.to_string();
	write!(
		stream,
		"HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
		body.len()
	)
	.map_err(|e| format!("http write: {e}"))
}

// ==== [sse] ====

// Server-sent events over HTTP: POST a JSON-RPC frame, the answer streams back
// as text/event-stream — one `data: {"token":...}` event per token, then a
// final `data: <response frame>` event, then the connection closes. Closing
// delimits the stream, so no chunked encoding. Thread-per-connection.

use serde_json::json;

fn serve_sse(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("sse bind: {e}"))?;
	for stream in listener.incoming() {
		let Ok(mut stream) = stream else { continue };
		let dispatch = dispatch.clone();
		std::thread::spawn(move || {
			let _ = respond_sse(&mut stream, dispatch.as_ref());
		});
	}
	Ok(())
}

fn respond_sse(stream: &mut TcpStream, dispatch: &dyn Dispatch) -> Result<(), String> {
	let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("sse: {e}"))?);

	let mut request_line = String::new();
	reader
		.read_line(&mut request_line)
		.map_err(|e| format!("sse read: {e}"))?;

	let mut content_length = 0usize;
	loop {
		let mut header = String::new();
		reader
			.read_line(&mut header)
			.map_err(|e| format!("sse read: {e}"))?;
		let header = header.trim();
		if header.is_empty() {
			break;
		}
		if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
			content_length = value.trim().parse().unwrap_or(0);
		}
	}

	if !request_line.starts_with("POST ") {
		return write!(
			stream,
			"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
		)
		.map_err(|e| format!("sse write: {e}"));
	}

	let mut body = vec![0u8; content_length];
	reader
		.read_exact(&mut body)
		.map_err(|e| format!("sse body: {e}"))?;

	write!(
		stream,
		"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n"
	)
	.map_err(|e| format!("sse write: {e}"))?;

	let frame = match serde_json::from_slice::<Value>(&body) {
		Ok(frame) => frame,
		Err(_) => {
			let event = error(Value::Null, -32700, "parse error");
			return write!(stream, "data: {event}\n\n").map_err(|e| format!("sse write: {e}"));
		}
	};

	let mut sink_error = None;
	let response = dispatch.call(&frame, &mut |token| {
		if sink_error.is_some() {
			return;
		}
		let event = json!({ "token": token });
		if let Err(e) = write!(stream, "data: {event}\n\n").and_then(|()| stream.flush()) {
			sink_error = Some(format!("sse write: {e}"));
		}
	});
	if let Some(e) = sink_error {
		return Err(e);
	}

	// A notification streams tokens (if any) but has no final frame to send.
	if let Some(response) = response {
		write!(stream, "data: {response}\n\n").map_err(|e| format!("sse write: {e}"))?;
	}
	Ok(())
}

// ==== [ws] ====

// WebSocket (RFC 6455) on std::net, no dependencies: SHA-1 and base64 for the
// handshake are hand-rolled below. A client text frame is a JSON-RPC request;
// the answer streams back as one {"token":...} text frame per token, closed by
// the response frame (or a -32700 error frame for an unparseable request). Ping
// is answered with pong, close with close. Thread-per-connection.

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn serve_ws(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
	let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("ws bind: {e}"))?;
	for stream in listener.incoming() {
		let Ok(mut stream) = stream else { continue };
		let dispatch = dispatch.clone();
		std::thread::spawn(move || {
			let _ = converse(&mut stream, dispatch.as_ref());
		});
	}
	Ok(())
}

fn converse(stream: &mut TcpStream, dispatch: &dyn Dispatch) -> Result<(), String> {
	let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("ws: {e}"))?);
	handshake(&mut reader, stream)?;

	loop {
		let (opcode, payload) = read_message(&mut reader)?;
		match opcode {
			// text
			1 => {
				let text = String::from_utf8_lossy(&payload);
				let response = match serde_json::from_str::<Value>(text.trim()) {
					Ok(frame) => {
						let mut sink_error = None;
						let out = dispatch.call(&frame, &mut |token| {
							if sink_error.is_some() {
								return;
							}
							let event = json!({ "token": token }).to_string();
							if let Err(e) = write_text(stream, event.as_bytes()) {
								sink_error = Some(e);
							}
						});
						if let Some(e) = sink_error {
							return Err(e);
						}
						out
					}
					Err(_) => Some(error(Value::Null, -32700, "parse error")),
				};
				// A notification answers with silence; a request answers its frame.
				if let Some(response) = response {
					write_text(stream, response.to_string().as_bytes())?;
				}
			}
			// close: echo and finish
			8 => {
				let _ = write_frame(stream, 0x88, &payload);
				return Ok(());
			}
			// ping → pong
			9 => write_frame(stream, 0x8A, &payload)?,
			_ => {}
		}
	}
}

fn handshake(reader: &mut BufReader<TcpStream>, stream: &mut TcpStream) -> Result<(), String> {
	let mut key = None;
	loop {
		let mut line = String::new();
		reader
			.read_line(&mut line)
			.map_err(|e| format!("ws handshake read: {e}"))?;
		let line = line.trim();
		if line.is_empty() {
			break;
		}
		if line.to_ascii_lowercase().starts_with("sec-websocket-key:") {
			// The key is case-sensitive base64; take it from the raw line.
			key = line.split_once(':').map(|(_, v)| v.trim().to_string());
		}
	}
	let key = key.ok_or("ws handshake: no Sec-WebSocket-Key header")?;
	let accept = base64(&sha1(format!("{key}{GUID}").as_bytes()));
	write!(
		stream,
		"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n"
	)
	.map_err(|e| format!("ws handshake write: {e}"))
}

// One complete message: continuation frames are joined until FIN. Control
// frames (close, ping) interleave and are returned as their own messages —
// they are never fragmented per the RFC.
fn read_message(reader: &mut BufReader<TcpStream>) -> Result<(u8, Vec<u8>), String> {
	let mut message: Vec<u8> = Vec::new();
	let mut message_opcode = 0u8;
	loop {
		let (fin, opcode, payload) = read_frame(reader)?;
		if opcode >= 8 {
			return Ok((opcode, payload));
		}
		if opcode != 0 {
			message_opcode = opcode;
		}
		message.extend_from_slice(&payload);
		if fin {
			return Ok((message_opcode, message));
		}
	}
}

fn read_frame(reader: &mut BufReader<TcpStream>) -> Result<(bool, u8, Vec<u8>), String> {
	let mut head = [0u8; 2];
	reader
		.read_exact(&mut head)
		.map_err(|e| format!("ws read: {e}"))?;
	let fin = head[0] & 0x80 != 0;
	let opcode = head[0] & 0x0F;
	let masked = head[1] & 0x80 != 0;
	let mut length = (head[1] & 0x7F) as u64;
	if length == 126 {
		let mut extended = [0u8; 2];
		reader
			.read_exact(&mut extended)
			.map_err(|e| format!("ws read: {e}"))?;
		length = u16::from_be_bytes(extended) as u64;
	} else if length == 127 {
		let mut extended = [0u8; 8];
		reader
			.read_exact(&mut extended)
			.map_err(|e| format!("ws read: {e}"))?;
		length = u64::from_be_bytes(extended);
	}
	let mask = if masked {
		let mut mask = [0u8; 4];
		reader
			.read_exact(&mut mask)
			.map_err(|e| format!("ws read: {e}"))?;
		Some(mask)
	} else {
		None
	};
	let mut payload = vec![0u8; length as usize];
	reader
		.read_exact(&mut payload)
		.map_err(|e| format!("ws read: {e}"))?;
	if let Some(mask) = mask {
		for (index, byte) in payload.iter_mut().enumerate() {
			*byte ^= mask[index % 4];
		}
	}
	Ok((fin, opcode, payload))
}

fn write_text(stream: &mut TcpStream, payload: &[u8]) -> Result<(), String> {
	write_frame(stream, 0x81, payload)
}

fn write_frame(stream: &mut TcpStream, head: u8, payload: &[u8]) -> Result<(), String> {
	let mut frame = vec![head];
	match payload.len() {
		0..=125 => frame.push(payload.len() as u8),
		126..=65535 => {
			frame.push(126);
			frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
		}
		_ => {
			frame.push(127);
			frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
		}
	}
	frame.extend_from_slice(payload);
	stream
		.write_all(&frame)
		.and_then(|()| stream.flush())
		.map_err(|e| format!("ws write: {e}"))
}

fn sha1(data: &[u8]) -> [u8; 20] {
	let mut state: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
	let mut message = data.to_vec();
	let bits = (data.len() as u64) * 8;
	message.push(0x80);
	while message.len() % 64 != 56 {
		message.push(0);
	}
	message.extend_from_slice(&bits.to_be_bytes());

	for block in message.chunks(64) {
		let mut words = [0u32; 80];
		for (index, chunk) in block.chunks(4).enumerate() {
			words[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
		}
		for index in 16..80 {
			words[index] = (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
				.rotate_left(1);
		}
		let (mut a, mut b, mut c, mut d, mut e) = (state[0], state[1], state[2], state[3], state[4]);
		for (index, word) in words.iter().enumerate() {
			let (f, k) = match index {
				0..=19 => ((b & c) | (!b & d), 0x5A827999u32),
				20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
				40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
				_ => (b ^ c ^ d, 0xCA62C1D6),
			};
			let next = a
				.rotate_left(5)
				.wrapping_add(f)
				.wrapping_add(e)
				.wrapping_add(k)
				.wrapping_add(*word);
			e = d;
			d = c;
			c = b.rotate_left(30);
			b = a;
			a = next;
		}
		state[0] = state[0].wrapping_add(a);
		state[1] = state[1].wrapping_add(b);
		state[2] = state[2].wrapping_add(c);
		state[3] = state[3].wrapping_add(d);
		state[4] = state[4].wrapping_add(e);
	}

	let mut digest = [0u8; 20];
	for (index, word) in state.iter().enumerate() {
		digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
	}
	digest
}

fn base64(data: &[u8]) -> String {
	const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
	let mut out = String::new();
	for chunk in data.chunks(3) {
		let bytes = [
			chunk[0],
			*chunk.get(1).unwrap_or(&0),
			*chunk.get(2).unwrap_or(&0),
		];
		let number = ((bytes[0] as u32) << 16) | ((bytes[1] as u32) << 8) | bytes[2] as u32;
		out.push(TABLE[(number >> 18) as usize & 63] as char);
		out.push(TABLE[(number >> 12) as usize & 63] as char);
		out.push(if chunk.len() > 1 {
			TABLE[(number >> 6) as usize & 63] as char
		} else {
			'='
		});
		out.push(if chunk.len() > 2 {
			TABLE[number as usize & 63] as char
		} else {
			'='
		});
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	// RFC 6455 section 1.3's own worked example.
	#[test]
	fn the_rfc_example_key_yields_the_rfc_example_accept() {
		let accept = base64(&sha1(format!("dGhlIHNhbXBsZSBub25jZQ=={GUID}").as_bytes()));
		assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
	}
}
