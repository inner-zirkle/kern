//! WS wire server: serves a [`super::Dispatch`] over this transport.

// WebSocket (RFC 6455) on std::net, no dependencies: SHA-1 and base64 for the
// handshake are hand-rolled below. A client text frame is a JSON-RPC request;
// the answer streams back as one {"token":...} text frame per token, closed by
// the response frame (or a -32700 error frame for an unparseable request). Ping
// is answered with pong, close with close. Thread-per-connection.

use super::{error, Dispatch};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub fn serve(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
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
