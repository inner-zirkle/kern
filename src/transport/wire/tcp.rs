// Newline-delimited JSON-RPC over TCP: one frame per line, one response line
// back. Each accepted connection is served on its own thread, so frames from
// different clients dispatch in parallel; the dispatcher is shared by Arc.

use super::{line_frame, Dispatch};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

pub fn serve(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
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
