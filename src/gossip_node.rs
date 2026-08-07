//! The gossip node: owns the peer table, socket loops, and send fan-out.
//! Every per-sender policy keys on the envelope-verified `PeerId`, never on
//! the spoofable `msg.origin` string.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use rand::seq::SliceRandom;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use crate::base_constants::*;
use crate::gossip_types::*;

use crate::gossip_identity::{loc_of, PeerId, PeerIdentity};
use crate::gossip_ledger::Ledger;
use crate::gossip_rate::{RateLimiter, GOSSIP_QUESTION_PER_MIN, GOSSIP_RATE_MAX_ORIGINS};
use crate::gossip_ring::{PeerEntry, RingView, RING_ENTRY_TTL_SECS, RING_JOIN_MAX_HOPS};
use crate::gossip_seen::SeenSet;
use crate::gossip_transport::{decode_msg, encode_msg, send_and_receive, send_msg};

// The PeerId is the envelope-verified sender — rate limits and any
// per-sender policy key on it, never on the spoofable `msg.origin` string.
pub type Handler = Arc<dyn Fn(PeerId, GossipMessage) + Send + Sync>;

pub type FetchHandler = Arc<dyn Fn(&str, &str) -> (Vec<u8>, bool) + Send + Sync>;

pub struct Node {
	pub addr: RwLock<String>,
	pub network_id: String,
	// The signing key every outbound frame travels under. Ephemeral unless the
	// daemon passes its persistent identity via `new_with_identity`.
	pub identity: Arc<PeerIdentity>,
	// Small-world ring view (FEDERATION_PLAN §2). None = legacy flat peers.
	ring: RwLock<Option<RingView>>,
	peers: RwLock<Vec<String>>,
	seen: SeenSet,
	// Unauthenticated peers, so the only lever on the Question oracle is cost.
	pub question_rate: RateLimiter,
	pub ledger: Ledger,
	lamport: AtomicU64,
	handler: RwLock<Option<Handler>>,
	fetch_handler: RwLock<Option<FetchHandler>>,
	stop_tx: watch::Sender<bool>,
	pub stop_rx: watch::Receiver<bool>,
}

impl Node {
	pub fn new(addr: &str, network_id: &str, peers: Vec<String>) -> Arc<Self> {
		Self::new_with_identity(addr, network_id, peers, Arc::new(PeerIdentity::generate()))
	}

	pub fn new_with_identity(
		addr: &str,
		network_id: &str,
		peers: Vec<String>,
		identity: Arc<PeerIdentity>,
	) -> Arc<Self> {
		let (stop_tx, stop_rx) = watch::channel(false);
		let mut peers = peers;
		peers.truncate(GOSSIP_MAX_PEERS);
		Arc::new(Self {
			addr: RwLock::new(addr.to_string()),
			network_id: network_id.to_string(),
			identity,
			ring: RwLock::new(None),
			peers: RwLock::new(peers),
			seen: SeenSet::new(),
			question_rate: RateLimiter::new(
				GOSSIP_QUESTION_PER_MIN,
				std::time::Duration::from_secs(60),
				GOSSIP_RATE_MAX_ORIGINS,
			),
			ledger: Ledger::new(),
			lamport: AtomicU64::new(0),
			handler: RwLock::new(None),
			fetch_handler: RwLock::new(None),
			stop_tx,
			stop_rx,
		})
	}

	pub fn set_handler(&self, h: Handler) {
		*self.handler.write() = Some(h);
	}

	/// Switch this node onto the ring: its location derives from its PeerId,
	/// its sampling seed from the same bytes so behaviour is reproducible.
	pub fn enable_ring(&self) {
		let id = self.identity.peer_id();
		let mut seed = [0u8; 8];
		seed.copy_from_slice(&id[8..16]);
		*self.ring.write() = Some(RingView::new(self.identity.loc(), u64::from_le_bytes(seed)));
	}

	pub fn ring_enabled(&self) -> bool {
		self.ring.read().is_some()
	}

	pub fn ring_peers(&self) -> Vec<PeerEntry> {
		match self.ring.read().as_ref() {
			Some(r) => r.near().iter().chain(r.far().iter()).cloned().collect(),
			None => Vec::new(),
		}
	}

	// Feed a verified sighting into the ring view. The id is envelope-verified;
	// the addr is the sender's claim — lying about it only breaks reachability
	// of the liar.
	fn ring_observe(&self, peer: PeerId, addr: &str) {
		if addr.is_empty() || peer == self.identity.peer_id() {
			return;
		}
		if let Some(r) = self.ring.write().as_mut() {
			r.observe(PeerEntry {
				id: peer,
				addr: addr.to_string(),
				loc: loc_of(&peer),
				last_seen: crate::util::now_secs(),
			});
		}
	}

	/// Greedy ring join (FEDERATION_PLAN §2): walk FindNearest hops toward our
	// own location from any bootstrap peer, adopting every neighborhood the
	// walk reveals. Iterative (requester-driven), so hops hold no state.
	pub async fn join_ring(self: &Arc<Self>, bootstrap: &[String]) {
		if !self.ring_enabled() {
			return;
		}
		let target = self.identity.loc();
		let mut contact = match bootstrap.first() {
			Some(a) => a.clone(),
			None => return,
		};
		for _ in 0..RING_JOIN_MAX_HOPS {
			let msg = GossipMessage {
				kind: GossipKind::FindNearest,
				id: format!("fn-{}-{}", self.addr(), now_nanos()),
				origin: self.addr(),
				payload: GossipPayload::FindNearest(FindNearestPayload { target }),
			};
			let reply = send_and_receive(&contact, &self.identity, self.bump_lamport(), &msg).await;
			let peers = match reply {
				Some(GossipMessage {
					payload: GossipPayload::Nearest(p),
					..
				}) => p.peers,
				_ => break,
			};
			let mut best: Option<(f64, String)> = None;
			{
				let mut ring = self.ring.write();
				let Some(r) = ring.as_mut() else { return };
				let own = crate::gossip_ring::ring_distance(r.loc(), target);
				for p in peers {
					if p.id == self.identity.peer_id() {
						continue;
					}
					let d = crate::gossip_ring::ring_distance(p.loc, target);
					if d < own && best.as_ref().map(|(b, _)| d < *b).unwrap_or(true) {
						best = Some((d, p.addr.clone()));
					}
					r.observe(p);
				}
			}
			match best {
				// A strictly closer peer exists: hop toward it.
				Some((_, next)) if next != contact => contact = next,
				// Terminal: the contact's neighborhood is ours to adopt.
				_ => break,
			}
		}
	}

	pub fn set_fetch_handler(&self, h: FetchHandler) {
		*self.fetch_handler.write() = Some(h);
	}

	pub fn addr(&self) -> String {
		self.addr.read().clone()
	}

	pub fn bump_lamport(&self) -> u64 {
		self.lamport.fetch_add(1, Ordering::SeqCst) + 1
	}

	pub fn observe_lamport(&self, remote: u64) {
		let mut current = self.lamport.load(Ordering::SeqCst);
		while remote > current {
			match self
				.lamport
				.compare_exchange(current, remote + 1, Ordering::SeqCst, Ordering::SeqCst)
			{
				Ok(_) => break,
				Err(actual) => current = actual,
			}
		}
	}

	pub fn add_peer(&self, addr: &str) {
		let mut peers = self.peers.write();
		if peers.len() >= GOSSIP_MAX_PEERS {
			return;
		}
		if !peers.iter().any(|p| p == addr) {
			peers.push(addr.to_string());
		}
	}

	pub fn peer_list(&self) -> Vec<String> {
		self.peers.read().clone()
	}

	pub fn peer_count(&self) -> usize {
		self.peers.read().len()
	}

	pub async fn listen(self: &Arc<Self>) -> Result<String, std::io::Error> {
		let addr = self.addr();
		let listener = TcpListener::bind(&addr).await?;
		let actual = listener.local_addr()?.to_string();
		*self.addr.write() = actual.clone();

		let node = self.clone();
		let mut stop = self.stop_rx.clone();
		tokio::spawn(async move {
			loop {
				tokio::select! {
					result = listener.accept() => {
						match result {
							Ok((stream, _)) => {
								let n = node.clone();
								tokio::spawn(async move { n.handle_conn(stream).await });
							}
							Err(_) => break,
						}
					}
					_ = stop.changed() => break,
				}
			}
		});

		Ok(actual)
	}

	pub fn close(&self) {
		let _ = self.stop_tx.send(true);
	}

	pub fn broadcast(self: &Arc<Self>, msg: GossipMessage) {
		if self.seen.add_and_check(&msg.id) {
			return;
		}
		self.forward(msg);
	}

	/// Fire-and-forget a signed frame to one peer — the primitive routed
	/// (tree/ring) traffic uses instead of gossip fanout.
	pub fn send_to(self: &Arc<Self>, addr: &str, msg: GossipMessage) {
		let identity = self.identity.clone();
		let lamport = self.bump_lamport();
		let addr = addr.to_string();
		tokio::spawn(async move {
			let _ = send_msg(&addr, &identity, lamport, &msg).await;
		});
	}

	/// The ring neighbor strictly closer to `target`, if any. None when the
	/// ring is disabled or we are terminal for this location.
	pub fn route_toward(&self, target: f64) -> Option<String> {
		self
			.ring
			.read()
			.as_ref()
			.and_then(|r| r.route(target).map(|e| e.addr.clone()))
	}

	pub async fn fetch_thought(&self, network_id: &str, entity_id: &str) -> Option<Vec<u8>> {
		let peer_addr = self
			.ledger
			.lookup_thought(entity_id)
			.or_else(|| self.ledger.lookup_routing(network_id))?;
		let msg = GossipMessage {
			kind: GossipKind::Fetch,
			id: format!("fetch-{entity_id}-{}", now_nanos()),
			origin: self.addr(),
			payload: GossipPayload::FetchRequest(FetchPayload {
				resource: "thought".into(),
				id: entity_id.into(),
			}),
		};
		match send_and_receive(&peer_addr, &self.identity, self.bump_lamport(), &msg).await {
			Some(reply) => {
				if let GossipPayload::FetchResult(r) = reply.payload {
					if r.found {
						Some(r.body)
					} else {
						None
					}
				} else {
					None
				}
			}
			None => None,
		}
	}

	pub fn start_heartbeat(self: &Arc<Self>) {
		let node = self.clone();
		let mut stop = self.stop_rx.clone();
		tokio::spawn(async move {
			let mut interval = tokio::time::interval(GOSSIP_HEARTBEAT_INTERVAL);
			loop {
				tokio::select! {
					_ = interval.tick() => {
						// Ring maintenance piggybacks on the heartbeat: evict the
						// silent; near repairs itself from far inside evict_stale.
						if let Some(r) = node.ring.write().as_mut() {
							r.evict_stale(crate::util::now_secs(), RING_ENTRY_TTL_SECS);
						}
						let msg = GossipMessage {
							kind: GossipKind::PeerExchange,
							id: format!("pe-{}-{}", node.addr(), now_nanos()),
							origin: node.addr(),
							payload: GossipPayload::PeerExchange(PeerExchangePayload {
								peers: node.peer_list(),
							}),
						};
						node.broadcast(msg);
					}
					_ = stop.changed() => break,
				}
			}
		});
	}

	async fn handle_conn(self: Arc<Self>, mut stream: TcpStream) {
		// decode_msg verifies the envelope signature; a forged frame returns
		// None HERE, before the seen-set, peer list, or any rate budget is
		// touched — invalid traffic buys no per-peer state.
		let (peer, msg) = match decode_msg(&mut stream).await {
			Some(m) => m,
			None => return,
		};

		if msg.kind == GossipKind::Fetch {
			self.handle_fetch(stream, msg).await;
			return;
		}

		if msg.kind == GossipKind::FindNearest {
			self.handle_find_nearest(stream, peer, msg).await;
			return;
		}

		if self.seen.add_and_check(&msg.id) {
			return;
		}

		if !msg.origin.is_empty() && msg.origin != self.addr() {
			self.add_peer(&msg.origin);
			self.ring_observe(peer, &msg.origin);
		}

		if let GossipPayload::Sphere(ref s) = msg.payload {
			self.ledger.put_routing(&s.kern_id, &msg.origin);
		}

		if let Some(h) = self.handler.read().as_ref() {
			h(peer, msg.clone());
		}

		// Contract traffic is routed along the subscription tree by its
		// handlers; gossip fanout would duplicate and mis-scope it.
		let routed = matches!(
			msg.kind,
			GossipKind::Subscribe
				| GossipKind::SubAck
				| GossipKind::ContractDelta
				| GossipKind::SyncSummary
				| GossipKind::SyncDiff
				| GossipKind::Tombstone
				| GossipKind::Nearest
		);
		if !routed {
			self.forward(msg);
		}
	}

	// Request/response like Fetch: reply with our own entry plus our `near`
	// set, so the joiner can keep hopping or adopt the neighborhood.
	async fn handle_find_nearest(&self, mut stream: TcpStream, peer: PeerId, msg: GossipMessage) {
		let _target = match &msg.payload {
			GossipPayload::FindNearest(p) => p.target,
			_ => return,
		};
		if !msg.origin.is_empty() {
			self.ring_observe(peer, &msg.origin);
		}
		let mut peers: Vec<PeerEntry> = Vec::new();
		if let Some(r) = self.ring.read().as_ref() {
			peers.push(PeerEntry {
				id: self.identity.peer_id(),
				addr: self.addr(),
				loc: r.loc(),
				last_seen: crate::util::now_secs(),
			});
			peers.extend(r.near().iter().cloned());
			peers.extend(r.far().iter().cloned());
		}
		let reply = GossipMessage {
			kind: GossipKind::Nearest,
			id: String::new(),
			origin: self.addr(),
			payload: GossipPayload::Nearest(NearestPayload { peers }),
		};
		let _ = encode_msg(&mut stream, &self.identity, self.bump_lamport(), &reply).await;
	}

	async fn handle_fetch(&self, mut stream: TcpStream, msg: GossipMessage) {
		let (resource, id) = if let GossipPayload::FetchRequest(ref f) = msg.payload {
			(f.resource.as_str(), f.id.as_str())
		} else {
			return;
		};

		let (body, found) = if let Some(fh) = self.fetch_handler.read().as_ref() {
			fh(resource, id)
		} else {
			(Vec::new(), false)
		};

		let reply = GossipMessage {
			kind: GossipKind::Fetch,
			id: String::new(),
			origin: self.addr(),
			payload: GossipPayload::FetchResult(FetchResultPayload { found, body }),
		};
		let _ = encode_msg(&mut stream, &self.identity, self.bump_lamport(), &reply).await;
	}

	fn forward(self: &Arc<Self>, msg: GossipMessage) {
		let peers = self.peer_list();
		let self_addr = self.addr();
		let mut candidates: Vec<&String> = peers
			.iter()
			.filter(|p| *p != &msg.origin && *p != &self_addr)
			.collect();

		let mut rng = rand::rng();
		candidates.shuffle(&mut rng);
		candidates.truncate(GOSSIP_FANOUT);

		for peer in candidates {
			let peer = peer.clone();
			let msg = msg.clone();
			let identity = self.identity.clone();
			let lamport = self.bump_lamport();
			tokio::spawn(async move {
				let _ = send_msg(&peer, &identity, lamport, &msg).await;
			});
		}
	}
}

use crate::util::now_nanos;

// ==== [discovery] ====

use std::net::{Ipv4Addr, SocketAddr};

use tokio::net::UdpSocket;

use crate::base_constants::{GOSSIP_DISCOVERY_INTERVAL, GOSSIP_DISCOVERY_MULTICAST};

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

	// Port 1 on loopback: refused instantly, no DNS and no off-host traffic.
	const DEAD_SEED: &str = "127.0.0.1:1";

	#[tokio::test]
	async fn an_unreachable_bootstrap_seed_never_blocks_or_panics_startup() {
		let node = Node::new("127.0.0.1:0", "net", vec![DEAD_SEED.into()]);
		let started = std::time::Instant::now();
		node.listen().await.expect("listener binds");
		node.start_heartbeat();
		node.broadcast(GossipMessage {
			kind: GossipKind::PeerExchange,
			id: "pe-1".into(),
			origin: node.addr(),
			payload: GossipPayload::PeerExchange(PeerExchangePayload {
				peers: node.peer_list(),
			}),
		});
		assert!(
			started.elapsed() < GOSSIP_DIAL_TIMEOUT,
			"a dead seed degrades in the background instead of stalling startup"
		);
		assert_eq!(node.peer_list(), vec![DEAD_SEED.to_string()]);
		node.close();
	}

	#[test]
	fn a_bootstrap_list_cannot_exceed_the_peer_cap() {
		let peers: Vec<String> = (0..GOSSIP_MAX_PEERS + 10)
			.map(|i| format!("10.0.0.{i}:7400"))
			.collect();
		let node = Node::new("127.0.0.1:0", "net", peers);
		assert_eq!(node.peer_count(), GOSSIP_MAX_PEERS);
		node.add_peer("10.9.9.9:7400");
		assert_eq!(node.peer_count(), GOSSIP_MAX_PEERS, "add_peer still capped");
	}

	fn pe_msg(id: &str, origin: &str) -> GossipMessage {
		GossipMessage {
			kind: GossipKind::PeerExchange,
			id: id.into(),
			origin: origin.into(),
			payload: GossipPayload::PeerExchange(PeerExchangePayload { peers: vec![] }),
		}
	}

	async fn ship_raw(addr: &str, frame: &SignedFrame) {
		use tokio::io::AsyncWriteExt;
		let bytes = bincode::serde::encode_to_vec(frame, bincode::config::standard()).unwrap();
		let mut s = TcpStream::connect(addr).await.unwrap();
		s.write_all(&(bytes.len() as u32).to_be_bytes())
			.await
			.unwrap();
		s.write_all(&bytes).await.unwrap();
		s.flush().await.unwrap();
	}

	#[tokio::test]
	async fn a_forged_frame_buys_no_state_and_never_reaches_the_handler() {
		let node = Node::new("127.0.0.1:0", "net", vec![]);
		let addr = node.listen().await.unwrap();
		let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
		let c = called.clone();
		node.set_handler(Arc::new(move |_peer, _msg| {
			c.store(true, Ordering::SeqCst);
		}));

		// Sign one body, ship another — the signature no longer covers it.
		let sender = PeerIdentity::generate();
		let honest =
			bincode::serde::encode_to_vec(pe_msg("h1", "10.0.0.9:1"), bincode::config::standard())
				.unwrap();
		let mut frame = sender.sign_frame(1, honest);
		frame.body =
			bincode::serde::encode_to_vec(pe_msg("forged", "10.0.0.9:1"), bincode::config::standard())
				.unwrap();
		let before = crate::gossip_identity::invalid_sig_dropped();
		ship_raw(&addr, &frame).await;
		tokio::time::sleep(std::time::Duration::from_millis(80)).await;

		assert!(
			!called.load(Ordering::SeqCst),
			"a forged frame must never reach the handler"
		);
		assert_eq!(
			node.peer_count(),
			0,
			"the forged origin never enters the peer list — verification precedes all per-peer state"
		);
		assert!(
			crate::gossip_identity::invalid_sig_dropped() > before,
			"the drop is counted"
		);
		node.close();
	}

	#[tokio::test]
	async fn join_ring_makes_two_nodes_each_others_ring_neighbors() {
		let a = Node::new("127.0.0.1:0", "net", vec![]);
		a.enable_ring();
		let a_addr = a.listen().await.unwrap();

		let b = Node::new("127.0.0.1:0", "net", vec![]);
		b.enable_ring();
		b.listen().await.unwrap();

		b.join_ring(&[a_addr]).await;

		let b_view = b.ring_peers();
		assert!(
			b_view.iter().any(|e| e.id == a.identity.peer_id()),
			"the joiner adopted the bootstrap peer into its ring view"
		);
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
		loop {
			let a_view = a.ring_peers();
			if a_view.iter().any(|e| e.id == b.identity.peer_id()) {
				break;
			}
			assert!(
				std::time::Instant::now() < deadline,
				"the contacted peer observes the verified joiner in return"
			);
			tokio::time::sleep(std::time::Duration::from_millis(10)).await;
		}
		a.close();
		b.close();
	}

	#[tokio::test]
	async fn an_honest_frame_reaches_the_handler_with_its_verified_peer_id() {
		let node = Node::new("127.0.0.1:0", "net", vec![]);
		let addr = node.listen().await.unwrap();
		let seen: Arc<RwLock<Option<crate::gossip_identity::PeerId>>> = Arc::new(RwLock::new(None));
		let s = seen.clone();
		node.set_handler(Arc::new(move |peer, _msg| {
			*s.write() = Some(peer);
		}));

		let sender = PeerIdentity::generate();
		let body =
			bincode::serde::encode_to_vec(pe_msg("h2", "10.0.0.9:2"), bincode::config::standard())
				.unwrap();
		ship_raw(&addr, &sender.sign_frame(1, body)).await;

		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
		loop {
			if let Some(peer) = *seen.read() {
				assert_eq!(peer, sender.peer_id(), "handler sees the signer's PeerId");
				break;
			}
			assert!(
				std::time::Instant::now() < deadline,
				"honest signed frame should be dispatched"
			);
			tokio::time::sleep(std::time::Duration::from_millis(10)).await;
		}
		node.close();
	}
}

#[cfg(test)]
mod discovery_tests {
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
