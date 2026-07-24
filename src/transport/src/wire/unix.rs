// Newline-delimited JSON-RPC over a Unix domain socket: local IPC, no network
// exposure, same line framing as tcp. A stale socket file from a prior run is
// removed before binding. Thread-per-connection, dispatcher shared by Arc.

use super::{line_frame, Dispatch};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::Arc;

pub fn serve(path: &str, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
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
