// Server-sent events over HTTP: POST a JSON-RPC frame, the answer streams back
// as text/event-stream — one `data: {"token":...}` event per token, then a
// final `data: <response frame>` event, then the connection closes. Closing
// delimits the stream, so no chunked encoding. Thread-per-connection.

use super::{error, Dispatch};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn serve(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("sse bind: {e}"))?;
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
