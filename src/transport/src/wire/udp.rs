// Datagram UDP: one datagram in is one JSON-RPC frame, one datagram back is the
// response frame. No streaming — a datagram has no stream — so tokens are
// dropped and the response frame carries the whole. An answer past 65507 bytes
// (the datagram ceiling) is refused with a -32000 frame rather than truncated,
// since a truncated JSON frame is unparseable. Single socket, served in a loop.

use super::{error, Dispatch};
use serde_json::Value;
use std::net::UdpSocket;
use std::sync::Arc;

const MAX_DATAGRAM: usize = 65507;

pub fn serve(port: u16, dispatch: Arc<dyn Dispatch>) -> Result<(), String> {
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
