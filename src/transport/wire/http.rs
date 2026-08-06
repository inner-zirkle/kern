// HTTP/1.1 on std::net: POST a JSON-RPC frame, receive its response frame as
// the body. No streaming — one request, one response. Thread-per-connection;
// the dispatcher is shared by Arc. 405 for anything but POST, 400 for a body
// that is not a frame.

use super::{error, Dispatch};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn serve(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("http bind: {e}"))?;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let dispatch = dispatch.clone();
        std::thread::spawn(move || {
            let _ = respond(&mut stream, dispatch.as_ref());
        });
    }
    Ok(())
}

fn respond(stream: &mut TcpStream, dispatch: &dyn Dispatch) -> Result<(), String> {
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
