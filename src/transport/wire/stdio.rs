//! STDIO wire server: serves a [`super::Dispatch`] over this transport.

// Newline-delimited JSON-RPC over stdin/stdout: MCP's own framing, and how a
// parent process spawns this surface as a child. One frame per line in, one
// response line out; a notification (no id) answers with silence. Serial by
// nature — stdio is a single stream — so no threads here.

use super::{line_frame, Dispatch};
use std::io::{self, BufRead, Write};
use std::sync::Arc;

pub fn serve(dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
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
