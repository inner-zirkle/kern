use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;

use crate::base_constants::{GOSSIP_DISCOVERY_INTERVAL, GOSSIP_DISCOVERY_MULTICAST};

use crate::gossip_node::Node;

const ANNOUNCE_PREFIX: &str = "kern:";

pub fn start_broadcast(node: &Arc<Node>, port: u16) {
	let node = node.clone();
	let addr: SocketAddr = match format!("{GOSSIP_DISCOVERY_MULTICAST}:{port}").parse() {
		Ok(a) => a,
		Err(_) => return,
	};
	tokio::spawn(async move {
		let socket = match UdpSocket::bind("0.0.0.0:0").await {
			Ok(s) => s,
			Err(_) => return,
		};

		let payload = format!("{ANNOUNCE_PREFIX}{}:{}", node.network_id, node.addr());
		let payload_bytes = payload.as_bytes();

		let mut interval = tokio::time::interval(GOSSIP_DISCOVERY_INTERVAL);
		let mut stop = node.stop_rx.clone();
		loop {
			tokio::select! {
				_ = interval.tick() => {
					let _ = socket.send_to(payload_bytes, addr).await;
				}
				_ = stop.changed() => break,
			}
		}
	});
}

pub fn start_listen(node: &Arc<Node>, port: u16) {
	let node = node.clone();
	tokio::spawn(async move {
		let group: Ipv4Addr = match GOSSIP_DISCOVERY_MULTICAST.parse() {
			Ok(g) => g,
			Err(_) => return,
		};
		let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).await {
			Ok(s) => s,
			Err(_) => return,
		};
		let _ = socket.join_multicast_v4(group, Ipv4Addr::UNSPECIFIED);
		let mut stop = node.stop_rx.clone();
		let mut buf = [0u8; 512];
		loop {
			tokio::select! {
				_ = stop.changed() => break,
				r = socket.recv_from(&mut buf) => {
					if let Ok((n, _src)) = r {
						if let Ok(s) = std::str::from_utf8(&buf[..n]) {
							if let Some((nid, addr)) = parse_announce(s) {
								if nid == node.network_id && addr != node.addr() {
									node.add_peer(&addr);
								}
							}
						}
					}
				}
			}
		}
	});
}

// ids never contain ':' (enforced by GossipConfig::effective_network_id), so split_once is safe.
pub fn parse_announce(s: &str) -> Option<(String, String)> {
	let s = s.strip_prefix(ANNOUNCE_PREFIX)?;
	let (network_id, tcp_addr) = s.split_once(':')?;
	if network_id.is_empty() || !tcp_addr.contains(':') {
		return None;
	}
	Some((network_id.to_string(), tcp_addr.to_string()))
}

#[cfg(test)]
mod tests {
	use super::*;

	const NID: &str = "123e4567-e89b-12d3-a456-426614174000";

	#[test]
	fn parse_announce_accepts_valid_payload() {
		let raw = format!("kern:{NID}:127.0.0.1:7400");
		let (nid, addr) = parse_announce(&raw).expect("valid announce parses");
		assert_eq!(nid, NID);
		assert_eq!(addr, "127.0.0.1:7400");
	}

	#[test]
	fn parse_announce_accepts_operator_configured_id() {
		let raw = "kern:team-alpha:10.0.0.5:7400";
		let (nid, addr) = parse_announce(raw).expect("custom id parses");
		assert_eq!(nid, "team-alpha");
		assert_eq!(addr, "10.0.0.5:7400");
	}

	#[test]
	fn parse_announce_rejects_wrong_prefix() {
		let raw = format!("gossip:{NID}:127.0.0.1:7400");
		assert!(
			parse_announce(&raw).is_none(),
			"non-kern prefix is rejected"
		);
	}

	#[test]
	fn parse_announce_rejects_missing_id_addr_separator() {
		assert!(parse_announce("kern:short").is_none());
	}

	#[test]
	fn parse_announce_rejects_addr_without_port_separator() {
		let raw = format!("kern:{NID}X127.0.0.1:7400");
		assert!(
			parse_announce(&raw).is_none(),
			"a mangled id/addr boundary is rejected"
		);
	}

	#[test]
	fn parse_announce_rejects_empty_id() {
		assert!(parse_announce("kern::127.0.0.1:7400").is_none());
	}
}
