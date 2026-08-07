//! The receive path: dispatch each verified message kind — anti-entropy
//! heartbeats, deltas, questions, contract traffic — into the local graph
//! through the merge/contract gates, with counters for everything refused.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::util::LogThrottle;
use std::sync::Arc;
use std::time::SystemTime;

use parking_lot::RwLock;

use crate::base_constants::*;
use crate::base_types::{Kern, ReasonKind};
use crate::crdt::{lww_wins, GCounter};
use crate::graph::GraphGnn;
use crate::search::search_all_unlocked;

use crate::gossip_contract::{
	contract_kern_id, contract_loc, entity_sig_digest, tombstone_digest, Applied, ContractId,
	ContractState, Delta as ContractDelta, ParamsV0, SignedCrdt, SignedEntity, SyncContract,
};
use crate::gossip_node::{FetchHandler, Handler, Node};
use crate::gossip_subs::SubTable;
use crate::gossip_types::*;

/// One hosted contract: the policy plus the signed bodies this node can
/// prove. Only hosts participate in a contract's subscription tree — every
/// hop validates, and validation needs the params.
pub struct ContractHost {
	pub params: ParamsV0,
	pub state: RwLock<ContractState>,
}

pub struct Deps {
	pub graph: Arc<RwLock<GraphGnn>>,
	pub node: Arc<Node>,
	pub queue: Option<Arc<crate::tick_queue::Queue>>,
	// Every federation mutation must call this or federated knowledge is lost on restart.
	pub save: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
	pub contracts: Arc<RwLock<std::collections::HashMap<ContractId, Arc<ContractHost>>>>,
	pub subs: Arc<SubTable>,
}

impl Deps {
	fn persist(&self) {
		if let Some(s) = &self.save {
			s();
		}
	}

	fn host(&self, cid: &ContractId) -> Option<Arc<ContractHost>> {
		self.contracts.read().get(cid).cloned()
	}
}

pub fn new_handler(d: Arc<Deps>) -> Handler {
	Arc::new(
		move |peer: crate::gossip_identity::PeerId, msg: GossipMessage| match msg.kind {
			GossipKind::Sphere => {
				if msg.id.starts_with("answer-") {
					handle_answer(&d, msg);
				} else {
					handle_sphere(&d, msg);
				}
			}
			GossipKind::Question => handle_question(&d, peer, msg),
			GossipKind::Pulse => handle_pulse(&d, msg),
			GossipKind::PeerExchange => handle_peer_exchange(&d, msg),
			GossipKind::Fetch => {}
			// Request/response kinds answered inside Node::handle_conn; nothing
			// for the graph handler to do.
			GossipKind::FindNearest | GossipKind::Nearest => {}
			GossipKind::Delta => handle_crdt_delta(&d, msg),
			GossipKind::EntitySync => handle_entity_sync(&d, msg),
			GossipKind::Subscribe => handle_subscribe(&d, msg),
			GossipKind::SubAck => handle_suback(&d, msg),
			GossipKind::ContractDelta | GossipKind::SyncDiff => handle_contract_delta(&d, msg),
			GossipKind::SyncSummary => handle_sync_summary(&d, msg),
			GossipKind::Tombstone => handle_tombstone(&d, msg),
		},
	)
}

pub fn wire_fetch(node: Arc<Node>, graph: Arc<RwLock<GraphGnn>>) {
	let handler: FetchHandler = Arc::new(move |resource: &str, id: &str| {
		if resource != "thought" {
			return (Vec::new(), false);
		}
		let g = graph.read();
		let found = g
			.kern_of_entity(id)
			.and_then(|kid| g.kerns.get(kid))
			.and_then(|k| k.entities.get(id));
		match found {
			Some(entity) => match bincode::serde::encode_to_vec(entity, bincode::config::standard()) {
				Ok(bytes) => (bytes, true),
				Err(_) => (Vec::new(), false),
			},
			None => (Vec::new(), false),
		}
	});
	node.set_fetch_handler(handler);
}

fn spawn_fetch_entity(d: &Arc<Deps>, network_id: String, kern_id: String, entity_id: String) {
	if d.graph.read().kern_of_entity(&entity_id).is_some() {
		return;
	}
	let d = d.clone();
	tokio::spawn(async move {
		let body = match d.node.fetch_thought(&network_id, &entity_id).await {
			Some(b) => b,
			None => return,
		};
		let entity = match bincode::serde::decode_from_slice::<crate::base_types::Entity, _>(
			&body,
			bincode::config::standard(),
		) {
			Ok((e, _)) => e,
			Err(_) => return,
		};
		// A peer answering with a different id than we asked for is a hijack attempt.
		if entity.id != entity_id {
			return;
		}
		let phantom = format!("remote-{network_id}-{kern_id}");
		let changed = {
			let mut g = d.graph.write();
			if !g.kerns.contains_key(&phantom) {
				let k = new_phantom_kern(&g, &phantom);
				g.register(k);
			}
			crate::merge::merge_remote_entity(&mut g, &phantom, entity)
		};
		if changed {
			d.persist();
		}
	});
}

pub fn start_announce(node: Arc<Node>, graph: Arc<RwLock<GraphGnn>>) {
	let mut stop = node.stop_rx.clone();
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(GOSSIP_HEARTBEAT_INTERVAL);
		loop {
			tokio::select! {
				_ = interval.tick() => {
					let payload = {
						let g = graph.read();
						if g.root.graviton_vec.is_empty() {
							None
						} else {
							Some(SpherePayload {
								network_id: g.network_id.clone(),
								kern_id: g.root.id.clone(),
								graviton_text: g.root.graviton_text.clone(),
								graviton_vec: g.root.graviton_vec.clone(),
								entity_id: String::new(),
								inner_radius: g.root.inner_radius,
								outer_radius: g.root.outer_radius,
							})
						}
					};
					if let Some(payload) = payload {
						let stamp = crate::util::now_nanos();
						let msg = GossipMessage {
							kind: GossipKind::Sphere,
							id: format!("sphere-{}-{}", node.addr(), stamp),
							origin: node.addr(),
							payload: GossipPayload::Sphere(payload),
						};
						node.broadcast(msg);
					}
				}
				_ = stop.changed() => break,
			}
		}
	});
}

const QUESTION_RATE_WARN_SECS: u64 = 300;
static QUESTION_RATE_WARN: LogThrottle = LogThrottle::new(QUESTION_RATE_WARN_SECS);

// Rows per heartbeat. Hard-coded rather than tuned: with no divergence estimate
// there is nothing to tune it against, and batch size is part of the anti-entropy
// question in `ROADMAP.md`, not a knob to guess at here.
const ENTITY_SYNC_BATCH: usize = 32;

// Select the hottest N, then clone only those. The previous version deep-cloned
// EVERY local entity and sorted the lot to keep 32 — O(N) clones plus O(N log N),
// every heartbeat, while holding the graph read lock, for a payload of fixed size.
// References cost a pointer each; `select_nth_unstable_by` is linear and uses the
// same total comparator as the old sort, so the chosen set and its order are
// unchanged.
fn hottest_local(g: &GraphGnn, n: usize) -> Vec<crate::base_types::Entity> {
	let mut refs: Vec<&crate::base_types::Entity> = g
		.kerns
		.iter()
		.filter(|(kid, _)| !crate::merge::is_remote_kern_id(kid))
		.flat_map(|(_, k)| k.entities.values())
		.collect();
	let by_heat = |a: &&crate::base_types::Entity, b: &&crate::base_types::Entity| {
		crate::util::cmp_rank(a.heat as f64, &a.id, b.heat as f64, &b.id)
	};
	if n == 0 {
		return Vec::new();
	}
	if refs.len() > n {
		refs.select_nth_unstable_by(n - 1, by_heat);
		refs.truncate(n);
	}
	refs.sort_by(by_heat);
	refs.into_iter().cloned().collect()
}

pub fn start_entity_sync(node: Arc<Node>, graph: Arc<RwLock<GraphGnn>>) {
	let mut stop = node.stop_rx.clone();
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(GOSSIP_HEARTBEAT_INTERVAL);
		loop {
			tokio::select! {
				_ = interval.tick() => {
					let payload = {
						let g = graph.read();
						let entities = hottest_local(&g, ENTITY_SYNC_BATCH);
						if entities.is_empty() {
							None
						} else {
							Some(EntitySyncPayload {
								network_id: g.network_id.clone(),
								kern_id: g.root.id.clone(),
								entities,
							})
						}
					};
					if let Some(payload) = payload {
						let stamp = crate::util::now_nanos();
						let msg = GossipMessage {
							kind: GossipKind::EntitySync,
							id: format!("esync-{}-{}", node.addr(), stamp),
							origin: node.addr(),
							payload: GossipPayload::EntitySync(payload),
						};
						node.broadcast(msg);
					}
				}
				_ = stop.changed() => break,
			}
		}
	});
}

pub fn start_delta_flush(node: Arc<Node>, graph: Arc<RwLock<GraphGnn>>) {
	let mut stop = node.stop_rx.clone();
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(GOSSIP_HEARTBEAT_INTERVAL);
		loop {
			tokio::select! {
				_ = interval.tick() => {
					let (deltas, kern_id) = {
						let g = graph.read();
						(g.drain_pending_deltas(), g.root.id.clone())
					};
					for delta in deltas {
						let target = match CrdtTarget::from_u8(delta.target) {
							Some(t) => t,
							None => continue,
						};
						let payload = CrdtDeltaPayload {
							kern_id: kern_id.clone(),
							object_id: delta.object_id,
							target,
							replica: delta.replica,
							value: delta.value,
							lamport: delta.lamport,
							producer: delta.producer,
							lww_value: delta.lww_value,
							orset_delta: Vec::new(),
						};
						let stamp = crate::util::now_nanos();
						let msg = GossipMessage {
							kind: GossipKind::Delta,
							id: format!("delta-{}-{}", node.addr(), stamp),
							origin: node.addr(),
							payload: GossipPayload::CrdtDelta(payload),
						};
						node.broadcast(msg);
					}
				}
				_ = stop.changed() => break,
			}
		}
	});
}

fn handle_sphere(d: &Deps, msg: GossipMessage) {
	let sphere = match &msg.payload {
		GossipPayload::Sphere(s) => s,
		_ => return,
	};

	if !sphere.network_id.is_empty() {
		let mut g = d.graph.write();
		if sphere.network_id != g.network_id {
			inject_remote_scope(&mut g, sphere, &msg.origin);
		}
		drop(g);
		d.node.ledger.put_routing(&sphere.network_id, &msg.origin);
		d.persist();
	}

	if let Some(q) = &d.queue {
		let g = d.graph.read();
		let root_id = g.root.id.clone();
		crate::tick_pulse::pulse(q, &g, &root_id, PULSE_THRESHOLD * 2.0);
	}
}

fn handle_answer(d: &Arc<Deps>, msg: GossipMessage) {
	let sphere = match &msg.payload {
		GossipPayload::Sphere(s) => s,
		_ => return,
	};

	let reason_id = msg.id.strip_prefix("answer-").unwrap_or(&msg.id);
	resolve_question_from_peer(d, reason_id, sphere, &msg.origin);

	if let Some(q) = &d.queue {
		let g = d.graph.read();
		let root_id = g.root.id.clone();
		crate::tick_pulse::pulse(q, &g, &root_id, PULSE_THRESHOLD * 2.0);
	}
}

fn handle_question(d: &Deps, peer: crate::gossip_identity::PeerId, msg: GossipMessage) {
	let question = match &msg.payload {
		GossipPayload::Question(q) => q,
		_ => return,
	};

	if question.reason_vec.is_empty() {
		return;
	}

	// SECURITY: answering tells the peer we hold something above the resolve
	// threshold for a vector THEY chose. That is a membership oracle, and the
	// content never has to be sent for it to leak. A budget makes bulk extraction
	// expensive. The budget is keyed on the envelope-verified PeerId — spoofing
	// a fresh `origin` string no longer buys a fresh budget; a fresh budget now
	// costs a fresh keypair.
	if !d.node.question_rate.allow(&crate::util::hex::encode(peer)) {
		if QUESTION_RATE_WARN.allow() {
			tracing::warn!(
				target: "kern.gossip",
				origin = %msg.origin,
				total_refused = d.node.question_rate.refused(),
				"peer exceeded its question budget; probes refused \
				 (further refusals counted, not logged)"
			);
		}
		return;
	}

	let g = d.graph.read();
	let hits = search_all_unlocked(&g, &question.reason_vec, 1);
	if hits.is_empty() || hits[0].score < QUESTION_RESOLVE_THRESHOLD {
		return;
	}

	let reply = GossipMessage {
		kind: GossipKind::Sphere,
		id: format!("answer-{}", question.reason_id),
		origin: d.node.addr(),
		payload: GossipPayload::Sphere(SpherePayload {
			network_id: g.network_id.clone(),
			kern_id: g.root.id.clone(),
			graviton_text: g.root.graviton_text.clone(),
			graviton_vec: g.root.graviton_vec.clone(),
			entity_id: hits[0].entity_id.clone(),
			inner_radius: g.root.inner_radius,
			outer_radius: g.root.outer_radius,
		}),
	};
	drop(g);

	d.node.broadcast(reply);
}

fn handle_pulse(d: &Deps, msg: GossipMessage) {
	let pulse = match &msg.payload {
		GossipPayload::Pulse(p) => p,
		_ => return,
	};
	let q = match &d.queue {
		Some(q) => q,
		None => return,
	};

	let g = d.graph.read();
	// SECURITY: an unknown kern id used to fall back to the LOCAL ROOT, so a peer
	// sending a garbage id drove maintenance straight into it. No design intent
	// justified that. Reject the id, confine the fan-out to `remote-*`, and clamp
	// the strength — the wire carries an arbitrary f64 and nothing else bounded it.
	if !crate::merge::is_remote_kern_id(&pulse.kern_id) {
		return;
	}
	if !g.kerns.contains_key(&pulse.kern_id) {
		return;
	}
	let strength = pulse.strength.clamp(0.0, MAX_REMOTE_PULSE);
	crate::tick_pulse::pulse(q, &g, &pulse.kern_id, strength);
}

fn handle_peer_exchange(d: &Deps, msg: GossipMessage) {
	let pe = match &msg.payload {
		GossipPayload::PeerExchange(p) => p,
		_ => return,
	};

	if !msg.origin.is_empty() {
		d.node.add_peer(&msg.origin);
	}

	let self_addr = d.node.addr();
	for peer in &pe.peers {
		if peer == &self_addr {
			continue;
		}
		if d.node.peer_count() >= GOSSIP_MAX_PEERS {
			break;
		}
		d.node.add_peer(peer);
	}
}

fn validated_delta_value(replica: &str, object_id: &str, value: u64) -> Option<u64> {
	if replica.is_empty() || object_id.is_empty() {
		return None;
	}
	if value == 0 || value > GOSSIP_CRDT_DELTA_MAX {
		return None;
	}
	Some(value)
}

fn handle_crdt_delta(d: &Deps, msg: GossipMessage) {
	let delta = match &msg.payload {
		GossipPayload::CrdtDelta(c) => c.clone(),
		_ => return,
	};

	let mut changed = false;
	{
		let mut g = d.graph.write();
		g.observe_lamport(delta.lamport);

		match delta.target {
			CrdtTarget::ThoughtAccessCount => {
				let value = match validated_delta_value(&delta.replica, &delta.object_id, delta.value) {
					Some(v) => v,
					None => return,
				};
				let mut incoming = GCounter::new();
				incoming.increment(&delta.replica, value);
				for kern_id in g.all_ids() {
					if let Some(kern) = g.get_mut(&kern_id) {
						if let Some(t) = kern.entities.get_mut(&delta.object_id) {
							changed = t.access_count.merge(&incoming);
							break;
						}
					}
				}
			}
			CrdtTarget::ReasonTraversalCount => {
				let value = match validated_delta_value(&delta.replica, &delta.object_id, delta.value) {
					Some(v) => v,
					None => return,
				};
				let mut incoming = GCounter::new();
				incoming.increment(&delta.replica, value);
				for kern_id in g.all_ids() {
					if let Some(kern) = g.get_mut(&kern_id) {
						if let Some(r) = kern.reasons.get_mut(&delta.object_id) {
							changed = r.traversal_count.merge(&incoming);
							break;
						}
					}
				}
			}
			CrdtTarget::ReasonScore => {
				let Some(score) = decode_lww::<f64>(&delta.lww_value) else {
					return;
				};
				for kern_id in remote_kern_ids(&g) {
					if let Some(kern) = g.get_mut(&kern_id) {
						if let Some(r) = kern.reasons.get_mut(&delta.object_id) {
							if lww_wins(
								(delta.lamport, &delta.producer),
								(r.score_lamport, &r.score_producer),
							) {
								r.score = score;
								r.score_lamport = delta.lamport;
								r.score_producer = delta.producer.clone();
								changed = true;
							}
							break;
						}
					}
				}
			}
			CrdtTarget::ValidUntil => {
				let Some(valid) = decode_lww::<Option<SystemTime>>(&delta.lww_value) else {
					return;
				};
				for kern_id in remote_kern_ids(&g) {
					if let Some(kern) = g.get_mut(&kern_id) {
						if let Some(t) = kern.entities.get_mut(&delta.object_id) {
							if lww_wins(
								(delta.lamport, &delta.producer),
								(t.valid_until_lamport, &t.valid_until_producer),
							) {
								t.valid_until = valid;
								t.valid_until_lamport = delta.lamport;
								t.valid_until_producer = delta.producer.clone();
								changed = true;
							}
							break;
						}
					}
				}
			}
			// SECURITY: never applied — see the statements note in `base::merge::merge_entity`.
			// No producer emits this target; it stays rejected rather than removed so a peer
			// on an older build cannot inject statement text under a content-addressed id.
			CrdtTarget::Statements => {}
		}
	}
	// Persist only after dropping the write guard — the save closure read-locks the graph (deadlock otherwise).
	if changed {
		d.persist();
	}
}

const MAX_REMOTE_PULSE: f64 = 1.0;
const FORGED_WARN_SECS: u64 = 300;
static FORGED_ID: AtomicU64 = AtomicU64::new(0);
static FORGED_WARN: LogThrottle = LogThrottle::new(FORGED_WARN_SECS);

// Remote bodies rejected because the text does not hash to the claimed id.
pub fn forged_id_rejected() -> u64 {
	FORGED_ID.load(Ordering::Relaxed)
}

// Every creation path mints `id = content_hash(text)` (`src/ingest/place.rs`,
// `src/ingest/file_watcher.rs`), and nothing in production ever appends to
// `statements`, so `text()` reproduces exactly what was hashed. Only ids of that
// SHAPE are judged: an id that is not a 64-char lowercase hex digest was never a
// content hash, so failing it would be an assertion about a format we do not
// define — and dropping legitimate remote knowledge is worse than the exposure.
pub(crate) fn id_matches_body(e: &crate::base_types::Entity) -> bool {
	let looks_hashed = e.id.len() == 64
		&& e
			.id
			.bytes()
			.all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
	if !looks_hashed {
		return true;
	}
	crate::util::content_hash(&e.text()) == e.id
}

// Only `remote-*` kerns may be reached by an unauthenticated LWW delta. Reaching a
// LOCAL row is intended for the G-Counters — ids are content hashes, so the same
// fact is a local row on both nodes and slot-max merging is what makes counts
// converge — but an LWW write buys federation nothing there and lets any peer
// overwrite local truth.
fn remote_kern_ids(g: &GraphGnn) -> Vec<String> {
	g.all_ids()
		.into_iter()
		.filter(|k| crate::merge::is_remote_kern_id(k))
		.collect()
}

fn decode_lww<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
	bincode::serde::decode_from_slice(bytes, bincode::config::standard())
		.ok()
		.map(|(v, _)| v)
}

// Content-addressing is the invariant every other federation guarantee rests on:
// it is why merge is safe as set-union and why a peer "cannot alter text you hold".
// A body whose text does not hash to its claimed id breaks all of it, so it is
// dropped on receipt. Cheap, and needs no authentication.
fn handle_entity_sync(d: &Deps, msg: GossipMessage) {
	let payload = match &msg.payload {
		GossipPayload::EntitySync(p) => p,
		_ => return,
	};
	if payload.network_id.is_empty() {
		return;
	}
	let mut g = d.graph.write();
	if payload.network_id == g.network_id {
		return;
	}
	let phantom = format!("remote-{}-{}", payload.network_id, payload.kern_id);
	if !g.kerns.contains_key(&phantom) {
		let k = new_phantom_kern(&g, &phantom);
		g.register(k);
	}
	let mut changed = false;
	for e in &payload.entities {
		if !id_matches_body(e) {
			let total = FORGED_ID.fetch_add(1, Ordering::Relaxed) + 1;
			if FORGED_WARN.allow() {
				tracing::warn!(
					target: "kern.gossip",
					origin = %msg.origin,
					id = %e.id,
					total_rejected = total,
					"remote entity body does not hash to its claimed id; dropped \
					 (further forgeries counted, not logged)"
				);
			}
			continue;
		}
		d.node.ledger.put_thought(&e.id, &msg.origin);
		changed |= crate::merge::merge_remote_entity(&mut g, &phantom, e.clone());
	}
	drop(g);
	if changed {
		d.persist();
	}
}

fn inject_remote_scope(g: &mut GraphGnn, sphere: &SpherePayload, _origin: &str) {
	let phantom_id = format!("remote-{}-{}", sphere.network_id, sphere.kern_id);

	if let Some(kern) = g.kerns.get_mut(&phantom_id) {
		kern.graviton_text = sphere.graviton_text.clone();
		kern.graviton_vec = sphere.graviton_vec.clone();
		kern.inner_radius = sphere.inner_radius;
		kern.outer_radius = sphere.outer_radius;
	} else {
		let mut k = new_phantom_kern(g, &phantom_id);
		k.graviton_text = sphere.graviton_text.clone();
		k.graviton_vec = sphere.graviton_vec.clone();
		k.inner_radius = sphere.inner_radius;
		k.outer_radius = sphere.outer_radius;
		g.register(k);
	}
}

fn new_phantom_kern(g: &GraphGnn, phantom_id: &str) -> Kern {
	let mut k = Kern::new(phantom_id, &g.root.id);
	k.root_id = g.root.root_id.clone();
	k
}

// ---- Contract kerns: subscription tree + delta sync (FEDERATION_PLAN §4) ----

static CONTRACT_DELTA_WARN: LogThrottle = LogThrottle::new(300);

fn ensure_contract_kern(g: &mut GraphGnn, cid: &ContractId) -> String {
	let kid = contract_kern_id(cid);
	if !g.kerns.contains_key(&kid) {
		let k = new_phantom_kern(g, &kid);
		g.register(k);
	}
	kid
}

// A Subscribe travels toward loc(ContractId); each hosting hop records the
// asker as downstream, answers with its summary, and (once) extends the tree
// toward the key. Non-hosts drop it: every tree hop validates deltas, and
// validation needs the params only hosts carry.
fn handle_subscribe(d: &Arc<Deps>, msg: GossipMessage) {
	let cid = match &msg.payload {
		GossipPayload::Subscribe(p) => p.contract,
		_ => return,
	};
	let Some(host) = d.host(&cid) else { return };
	if msg.origin.is_empty() || msg.origin == d.node.addr() {
		return;
	}
	d.subs.add_downstream(&cid, &msg.origin);

	let summary = SignedCrdt.summarize(&host.state.read());
	d.node.send_to(
		&msg.origin,
		GossipMessage {
			kind: GossipKind::SubAck,
			id: format!("suback-{}-{}", d.node.addr(), crate::util::now_nanos()),
			origin: d.node.addr(),
			payload: GossipPayload::SubAck(SubAckPayload {
				contract: cid,
				summary,
			}),
		},
	);

	// Extend the tree toward the key once; the peer closest to loc(cid) ends
	// up the root because route() returns None there.
	if d.subs.upstream(&cid).is_none() {
		subscribe_upstream(d, &cid);
	}
}

// Resolve and dial our tree parent for a contract: the ring neighbor
// strictly closer to the key, if any.
pub fn subscribe_upstream(d: &Arc<Deps>, cid: &ContractId) {
	let Some(next) = d.node.route_toward(contract_loc(cid)) else {
		return;
	};
	// Never adopt our own subscriber as parent — that closes a cycle that
	// starves everyone outside it.
	if next == d.node.addr() || d.subs.is_downstream(cid, &next) {
		return;
	}
	d.node.send_to(
		&next,
		GossipMessage {
			kind: GossipKind::Subscribe,
			id: format!("sub-{}-{}", d.node.addr(), crate::util::now_nanos()),
			origin: d.node.addr(),
			payload: GossipPayload::Subscribe(SubscribePayload { contract: *cid }),
		},
	);
}

// The parent acknowledged: adopt it as upstream and reconcile both ways —
// push what it misses, show it our summary so it pushes what we miss.
fn handle_suback(d: &Arc<Deps>, msg: GossipMessage) {
	let (cid, their_summary) = match &msg.payload {
		GossipPayload::SubAck(p) => (p.contract, p.summary.clone()),
		_ => return,
	};
	let Some(host) = d.host(&cid) else { return };
	if msg.origin.is_empty() || d.subs.is_downstream(&cid, &msg.origin) {
		return;
	}
	// First parent wins: a later SubAck (a raced tree extension) must not
	// re-root us mid-flight; eviction and the sync loop handle real repair.
	match d.subs.upstream(&cid) {
		Some(existing) if existing != msg.origin => return,
		_ => d.subs.set_upstream(&cid, Some(msg.origin.clone())),
	}

	let (ours, our_summary) = {
		let state = host.state.read();
		(
			SignedCrdt.diff(&state, &their_summary),
			SignedCrdt.summarize(&state),
		)
	};
	if !ours.entities.is_empty() {
		d.node.send_to(
			&msg.origin,
			GossipMessage {
				kind: GossipKind::SyncDiff,
				id: format!("sdiff-{}-{}", d.node.addr(), crate::util::now_nanos()),
				origin: d.node.addr(),
				payload: GossipPayload::SyncDiff(SyncDiffPayload {
					contract: cid,
					delta: ours,
				}),
			},
		);
	}
	if our_summary.root != their_summary.root {
		d.node.send_to(
			&msg.origin,
			GossipMessage {
				kind: GossipKind::SyncSummary,
				id: format!("ssum-{}-{}", d.node.addr(), crate::util::now_nanos()),
				origin: d.node.addr(),
				payload: GossipPayload::SyncSummary(SyncSummaryPayload {
					contract: cid,
					summary: our_summary,
				}),
			},
		);
	}
}

// Anti-entropy pull: a peer showed us its summary; ship exactly what it
// lacks. Convergence terminates the exchange — equal roots diff to empty.
fn handle_sync_summary(d: &Arc<Deps>, msg: GossipMessage) {
	let (cid, their_summary) = match &msg.payload {
		GossipPayload::SyncSummary(p) => (p.contract, p.summary.clone()),
		_ => return,
	};
	let Some(host) = d.host(&cid) else { return };
	if msg.origin.is_empty() {
		return;
	}
	let missing = SignedCrdt.diff(&host.state.read(), &their_summary);
	if missing.entities.is_empty() {
		return;
	}
	d.node.send_to(
		&msg.origin,
		GossipMessage {
			kind: GossipKind::SyncDiff,
			id: format!("sdiff-{}-{}", d.node.addr(), crate::util::now_nanos()),
			origin: d.node.addr(),
			payload: GossipPayload::SyncDiff(SyncDiffPayload {
				contract: cid,
				delta: missing,
			}),
		},
	);
}

// A delta arrived (live tree flood or anti-entropy diff — same admission
// path): validate against the contract, apply through the CRDT merge, and
// forward along the tree only when it changed something here.
fn handle_contract_delta(d: &Arc<Deps>, msg: GossipMessage) {
	let (cid, delta) = match &msg.payload {
		GossipPayload::ContractDelta(p) => (p.contract, p.delta.clone()),
		GossipPayload::SyncDiff(p) => (p.contract, p.delta.clone()),
		_ => return,
	};
	let Some(host) = d.host(&cid) else { return };

	let applied = {
		let mut state = host.state.write();
		if let Err(refusal) = SignedCrdt.validate_delta(&host.params, &state, &delta) {
			if CONTRACT_DELTA_WARN.allow() {
				tracing::warn!(
					target: "kern.gossip",
					origin = %msg.origin,
					contract = %crate::util::short_id(&crate::util::hex::encode(cid)),
					refusal = ?refusal,
					total_refused = crate::gossip_contract::contract_refused(),
					"contract delta refused (further refusals counted, not logged)"
				);
			}
			return;
		}
		let mut g = d.graph.write();
		let kid = ensure_contract_kern(&mut g, &cid);
		SignedCrdt.apply(&mut g, &kid, &host.params, &mut state, delta.clone())
	};

	if applied.changed {
		d.persist();
		// Natural flood suppression: only news travels further.
		for addr in d.subs.fanout(&cid, &msg.origin) {
			d.node.send_to(
				&addr,
				GossipMessage {
					kind: GossipKind::ContractDelta,
					id: format!("cdelta-{}-{}", d.node.addr(), crate::util::now_nanos()),
					origin: d.node.addr(),
					payload: GossipPayload::ContractDelta(ContractDeltaPayload {
						contract: cid,
						delta: delta.clone(),
					}),
				},
			);
		}
	}
}

// An owner retired the contract in favour of an amended one. Verify the
// owner signature over (old, new); v0 drops the subscription and logs the
// forward pointer — following it is the operator's call.
fn handle_tombstone(d: &Arc<Deps>, msg: GossipMessage) {
	let p = match &msg.payload {
		GossipPayload::Tombstone(p) => p.clone(),
		_ => return,
	};
	let Some(host) = d.host(&p.contract) else {
		return;
	};
	let digest = tombstone_digest(&p.contract, &p.new_id);
	let signed_by_owner = host
		.params
		.owners
		.iter()
		.any(|o| crate::gossip_identity::verify_sig_by(o, &digest, &p.sig));
	if !signed_by_owner {
		return;
	}
	d.subs.remove(&p.contract);
	tracing::info!(
		target: "kern.gossip",
		old = %crate::util::short_id(&crate::util::hex::encode(p.contract)),
		new = %crate::util::short_id(&crate::util::hex::encode(p.new_id)),
		"contract tombstoned by its owner; unsubscribed — resubscribe to the new id to follow"
	);
}

/// Local ingest into a shared kern: sign the entity with the daemon's key,
/// admit it through the same validation peers apply, then flood the delta
/// along the tree. Refusals surface to the caller — publishing something
/// the contract refuses locally would only be refused remotely anyway.
pub fn publish_to_contract(
	d: &Arc<Deps>,
	cid: &ContractId,
	entity: crate::base_types::Entity,
) -> Result<Applied, String> {
	let Some(host) = d.host(cid) else {
		return Err("contract not hosted on this node".into());
	};
	let lamport = d.node.bump_lamport();
	let digest = entity_sig_digest(&entity.id, lamport);
	let se = SignedEntity {
		sig: d.node.identity.sign_digest(&digest),
		signer: d.node.identity.pubkey(),
		lamport,
		entity,
	};
	let delta = ContractDelta { entities: vec![se] };
	let applied = {
		let mut state = host.state.write();
		SignedCrdt
			.validate_delta(&host.params, &state, &delta)
			.map_err(|r| format!("contract refused the publish: {r:?}"))?;
		let mut g = d.graph.write();
		let kid = ensure_contract_kern(&mut g, cid);
		SignedCrdt.apply(&mut g, &kid, &host.params, &mut state, delta.clone())
	};
	if applied.changed {
		d.persist();
	}
	for addr in d.subs.fanout(cid, "") {
		d.node.send_to(
			&addr,
			GossipMessage {
				kind: GossipKind::ContractDelta,
				id: format!("cdelta-{}-{}", d.node.addr(), crate::util::now_nanos()),
				origin: d.node.addr(),
				payload: GossipPayload::ContractDelta(ContractDeltaPayload {
					contract: *cid,
					delta: delta.clone(),
				}),
			},
		);
	}
	Ok(applied)
}

/// One anti-entropy pass: show our summary to each tree parent (they answer
/// with a SyncDiff of what we lack), and re-dial a parent for any contract
/// still rootless. Called on the sync interval and by tests.
pub fn contract_sync_once(d: &Arc<Deps>) {
	let contracts: Vec<ContractId> = d.contracts.read().keys().copied().collect();
	for cid in contracts {
		let Some(host) = d.host(&cid) else { continue };
		match d.subs.upstream(&cid) {
			Some(up) => {
				let summary = SignedCrdt.summarize(&host.state.read());
				d.node.send_to(
					&up,
					GossipMessage {
						kind: GossipKind::SyncSummary,
						id: format!("ssum-{}-{}", d.node.addr(), crate::util::now_nanos()),
						origin: d.node.addr(),
						payload: GossipPayload::SyncSummary(SyncSummaryPayload {
							contract: cid,
							summary,
						}),
					},
				);
			}
			None => subscribe_upstream(d, &cid),
		}
	}
}

/// The periodic anti-entropy driver (FEDERATION_PLAN §4): every
/// `sync_interval_secs`, reconcile with tree parents.
pub fn start_contract_sync(d: Arc<Deps>, interval_secs: u64) {
	let mut stop = d.node.stop_rx.clone();
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs.max(1)));
		loop {
			tokio::select! {
				_ = interval.tick() => contract_sync_once(&d),
				_ = stop.changed() => break,
			}
		}
	});
}

fn resolve_question_from_peer(
	d: &Arc<Deps>,
	reason_id: &str,
	sphere: &SpherePayload,
	origin: &str,
) {
	if sphere.entity_id.is_empty() || sphere.network_id.is_empty() {
		return;
	}

	let (reason, kern_id, local_net) = {
		let g = d.graph.read();
		match crate::search::find_reason(&g, reason_id) {
			Some((reason, kern_id)) => (reason, kern_id, g.network_id.clone()),
			None => return,
		}
	};
	if reason.kind != ReasonKind::Question || !reason.to.is_empty() {
		return;
	}

	let is_local = sphere.network_id == local_net;

	let mut g = d.graph.write();
	if let Some(kern) = g.kerns.get_mut(&kern_id) {
		if let Some(r) = kern.reasons.get_mut(reason_id) {
			r.to = sphere.entity_id.clone();
			r.kind = ReasonKind::Similarity;
			if !is_local {
				r.to_net_id = sphere.network_id.clone();
				d.node.ledger.put_routing(&sphere.network_id, origin);
				d.node.ledger.put_thought(&sphere.entity_id, origin);
			}
			r.id = crate::math::reason_id(&r.from, &r.to, r.kind, &r.text, &r.to_net_id);
		}
	}
	drop(g);

	// The answer only names the entity; without the body the cross-net reason dangles.
	if !is_local {
		spawn_fetch_entity(
			d,
			sphere.network_id.clone(),
			sphere.kern_id.clone(),
			sphere.entity_id.clone(),
		);
	}

	if let Some(q) = &d.queue {
		q.enqueue(crate::tick_queue::task(
			crate::tick_queue::TaskKind::Persist,
			&kern_id,
		));
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base_types::{mk_entity as mk_entity_kind, Entity, EntityKind, Reason};
	use crate::reason::add_reason;

	fn mk_entity(id: &str, text: &str, heat: f64) -> Entity {
		mk_entity_kind(id, text, heat, EntityKind::Fact)
	}

	fn mk_deps(graph: Arc<RwLock<GraphGnn>>) -> Deps {
		Deps {
			graph,
			node: Node::new("127.0.0.1:0", "testnet", vec![]),
			queue: None,
			save: None,
			contracts: Arc::new(RwLock::new(std::collections::HashMap::new())),
			subs: Arc::new(SubTable::new()),
		}
	}

	fn esync_msg(network_id: &str, kern_id: &str, entities: Vec<Entity>) -> GossipMessage {
		GossipMessage {
			kind: GossipKind::EntitySync,
			id: "esync-test".to_string(),
			origin: "127.0.0.1:1".to_string(),
			payload: GossipPayload::EntitySync(EntitySyncPayload {
				network_id: network_id.to_string(),
				kern_id: kern_id.to_string(),
				entities,
			}),
		}
	}

	#[test]
	fn entity_sync_merges_remote_body_into_phantom() {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let d = mk_deps(g.clone());

		let msg = esync_msg(
			"othernet",
			"rootK",
			vec![mk_entity("eR", "remote thought", 3.0)],
		);
		handle_entity_sync(&d, msg);

		let guard = g.read();
		let phantom = "remote-othernet-rootK";
		let kern = guard.kerns.get(phantom).expect("phantom kern created");
		assert!(
			kern.entities.contains_key("eR"),
			"remote entity merged into phantom"
		);
		assert_eq!(guard.kern_of_entity("eR"), Some(phantom));
	}

	#[test]
	fn entity_sync_ignores_same_network() {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let own_net = g.read().network_id.clone();
		let d = mk_deps(g.clone());

		let msg = esync_msg(&own_net, "rootK", vec![mk_entity("eR", "echo", 3.0)]);
		handle_entity_sync(&d, msg);

		let guard = g.read();
		assert!(
			guard.kern_of_entity("eR").is_none(),
			"own-network echo ignored"
		);
		assert!(
			!guard.kerns.keys().any(|k| k.starts_with("remote-")),
			"no phantom kern created for own data"
		);
	}

	#[test]
	fn delta_validation_accepts_sane_value() {
		assert_eq!(validated_delta_value("r1", "obj", 5), Some(5));
		assert_eq!(
			validated_delta_value("r1", "obj", GOSSIP_CRDT_DELTA_MAX),
			Some(GOSSIP_CRDT_DELTA_MAX)
		);
	}

	#[test]
	fn delta_validation_drops_empty_ids_and_zero() {
		assert_eq!(validated_delta_value("", "obj", 5), None);
		assert_eq!(validated_delta_value("r1", "", 5), None);
		assert_eq!(validated_delta_value("r1", "obj", 0), None);
	}

	#[test]
	fn delta_validation_rejects_oversized_value() {
		assert_eq!(
			validated_delta_value("r1", "obj", GOSSIP_CRDT_DELTA_MAX + 1),
			None
		);
		assert_eq!(validated_delta_value("r1", "obj", u64::MAX), None);
	}

	#[test]
	fn peer_exchange_caps_at_max_peers() {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let d = mk_deps(g);
		let peers: Vec<String> = (0..100).map(|i| format!("10.0.0.{i}:7400")).collect();
		let msg = GossipMessage {
			kind: GossipKind::PeerExchange,
			id: "pe-test".to_string(),
			origin: "127.0.0.1:1".to_string(),
			payload: GossipPayload::PeerExchange(PeerExchangePayload { peers }),
		};
		handle_peer_exchange(&d, msg);
		assert_eq!(
			d.node.peer_count(),
			GOSSIP_MAX_PEERS,
			"peer table is capped at GOSSIP_MAX_PEERS"
		);
	}

	fn mk_deps_with_save(
		graph: Arc<RwLock<GraphGnn>>,
		calls: Arc<std::sync::atomic::AtomicUsize>,
	) -> Deps {
		Deps {
			graph,
			node: Node::new("127.0.0.1:0", "testnet", vec![]),
			queue: None,
			save: Some(Arc::new(move || {
				calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
			})),
			contracts: Arc::new(RwLock::new(std::collections::HashMap::new())),
			subs: Arc::new(SubTable::new()),
		}
	}

	fn delta_msg(target: CrdtTarget, object_id: &str, replica: &str, value: u64) -> GossipMessage {
		GossipMessage {
			kind: GossipKind::Delta,
			id: "delta-test".to_string(),
			origin: "127.0.0.1:1".to_string(),
			payload: GossipPayload::CrdtDelta(CrdtDeltaPayload {
				kern_id: "k".to_string(),
				object_id: object_id.to_string(),
				target,
				replica: replica.to_string(),
				value,
				lamport: 0,
				producer: String::new(),
				lww_value: Vec::new(),
				orset_delta: Vec::new(),
			}),
		}
	}

	fn graph_with_one_entity(id: &str) -> Arc<RwLock<GraphGnn>> {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert(id.to_string(), mk_entity(id, "t", 1.0));
		g.kerns.insert("k".to_string(), k);
		Arc::new(RwLock::new(g))
	}

	#[test]
	fn crdt_delta_merges_counter_and_persists() {
		use std::sync::atomic::{AtomicUsize, Ordering};
		let g = graph_with_one_entity("e");
		let calls = Arc::new(AtomicUsize::new(0));
		let d = mk_deps_with_save(g.clone(), calls.clone());

		handle_crdt_delta(
			&d,
			delta_msg(CrdtTarget::ThoughtAccessCount, "e", "peerR", 7),
		);

		let merged = g.read().kerns["k"].entities["e"].access_count.value();
		assert_eq!(merged, 7, "the remote replica's count is merged in");
		assert_eq!(
			calls.load(Ordering::SeqCst),
			1,
			"a counter merge persists per the Deps contract (was silently dropped)"
		);
	}

	#[test]
	fn crdt_delta_idempotent_redelta_does_not_persist_again() {
		use std::sync::atomic::{AtomicUsize, Ordering};
		let g = graph_with_one_entity("e");
		let calls = Arc::new(AtomicUsize::new(0));
		let d = mk_deps_with_save(g.clone(), calls.clone());

		handle_crdt_delta(
			&d,
			delta_msg(CrdtTarget::ThoughtAccessCount, "e", "peerR", 5),
		);
		handle_crdt_delta(
			&d,
			delta_msg(CrdtTarget::ThoughtAccessCount, "e", "peerR", 5),
		);
		assert_eq!(
			calls.load(Ordering::SeqCst),
			1,
			"only the changing merge persists"
		);
	}

	fn graph_with_open_question() -> (Arc<RwLock<GraphGnn>>, String) {
		let mut g = GraphGnn::new();
		let net = g.network_id.clone();
		let mut k = Kern::new("kq", "");
		add_reason(
			&mut k,
			Reason {
				from: "a".into(),
				to: String::new(),
				id: "r1".into(),
				kind: ReasonKind::Question,
				..Default::default()
			},
		);
		g.kerns.insert("kq".into(), k);
		(Arc::new(RwLock::new(g)), net)
	}

	fn answer_sphere(net: &str, entity_id: &str) -> SpherePayload {
		SpherePayload {
			network_id: net.to_string(),
			kern_id: "rk".into(),
			graviton_vec: vec![],
			graviton_text: String::new(),
			entity_id: entity_id.to_string(),
			inner_radius: 0.0,
			outer_radius: 0.0,
		}
	}

	#[tokio::test]
	async fn fetch_thought_round_trips_an_entity_body_from_the_holder() {
		let holder_graph = Arc::new(RwLock::new(GraphGnn::new()));
		{
			let mut g = holder_graph.write();
			let root = g.root.id.clone();
			let mut k = Kern::new("kh", &root);
			k.entities
				.insert("eF".into(), mk_entity("eF", "fetched body", 2.0));
			g.register(k);
		}
		let holder = Node::new("127.0.0.1:0", "holdernet", vec![]);
		wire_fetch(holder.clone(), holder_graph.clone());
		let holder_addr = holder.listen().await.expect("holder listens");

		let seeker = Node::new("127.0.0.1:0", "seekernet", vec![]);
		seeker.ledger.put_thought("eF", &holder_addr);

		let body = seeker
			.fetch_thought("holdernet", "eF")
			.await
			.expect("holder serves the entity body");
		let (entity, _) = bincode::serde::decode_from_slice::<crate::base_types::Entity, _>(
			&body,
			bincode::config::standard(),
		)
		.expect("body decodes as an Entity");
		assert_eq!(entity.id, "eF");
		assert_eq!(entity.text(), "fetched body");

		assert!(
			seeker.fetch_thought("holdernet", "missing").await.is_none(),
			"an unknown id resolves to not-found, not a bogus body"
		);
	}

	#[test]
	fn resolve_question_from_peer_fills_answer_and_promotes_to_similarity() {
		let (g, net) = graph_with_open_question();
		let d = Arc::new(mk_deps(g.clone()));
		resolve_question_from_peer(&d, "r1", &answer_sphere(&net, "ans"), "127.0.0.1:9");
		let guard = g.read();
		let r = guard.kerns["kq"].reasons.get("r1").expect("reason present");
		assert_eq!(r.to, "ans", "answer endpoint filled in");
		assert!(
			matches!(r.kind, ReasonKind::Similarity),
			"open question promoted to similarity"
		);
	}

	#[test]
	fn resolve_question_from_peer_ignores_an_empty_answer() {
		let (g, net) = graph_with_open_question();
		let d = Arc::new(mk_deps(g.clone()));
		resolve_question_from_peer(&d, "r1", &answer_sphere(&net, ""), "o");
		let guard = g.read();
		let r = guard.kerns["kq"].reasons.get("r1").unwrap();
		assert!(
			r.to.is_empty() && matches!(r.kind, ReasonKind::Question),
			"an empty answer leaves the question untouched",
		);
	}

	#[test]
	fn handle_question_with_empty_reason_vec_is_a_noop() {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let d = mk_deps(g.clone());
		let msg = GossipMessage {
			kind: GossipKind::Question,
			id: "q".into(),
			origin: "o".into(),
			payload: GossipPayload::Question(QuestionPayload {
				reason_id: "r".into(),
				reason_vec: vec![],
				question_text: String::new(),
			}),
		};
		handle_question(&d, [1u8; 32], msg);
	}

	#[test]
	fn handle_pulse_falls_back_to_root_for_an_unknown_kern() {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let q = Arc::new(crate::tick_queue::Queue::new(64));
		let d = Deps {
			graph: g.clone(),
			node: Node::new("127.0.0.1:0", "testnet", vec![]),
			queue: Some(q),
			save: None,
			contracts: Arc::new(RwLock::new(std::collections::HashMap::new())),
			subs: Arc::new(SubTable::new()),
		};
		let msg = GossipMessage {
			kind: GossipKind::Pulse,
			id: "p".into(),
			origin: "o".into(),
			payload: GossipPayload::Pulse(PulsePayload {
				kern_id: "does-not-exist".into(),
				strength: 1.0,
			}),
		};
		handle_pulse(&d, msg);
	}
	#[test]
	fn a_real_ingested_entity_satisfies_the_id_body_check() {
		// The guard's blast radius: if this invariant does not hold on the real
		// creation path, the check drops legitimate remote knowledge. Build through
		// the shipped constructor rather than by hand.
		let text = "Ada keeps her bicycle in the garden shed";
		let e = crate::base_types::Entity {
			id: crate::util::content_hash(text),
			statements: vec![text.to_string()],
			chunks: vec![crate::base_types::ChunkPart {
				kind: crate::base_types::ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		};
		assert_eq!(e.text(), text, "text() must reproduce what was hashed");
		assert!(
			id_matches_body(&e),
			"a legitimate entity must never be dropped"
		);
	}

	#[test]
	fn a_body_that_does_not_hash_to_its_id_is_rejected() {
		let mut e = crate::base_types::Entity {
			id: crate::util::content_hash("the original text"),
			statements: vec!["attacker-substituted text".to_string()],
			chunks: vec![crate::base_types::ChunkPart {
				kind: crate::base_types::ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		};
		assert!(
			!id_matches_body(&e),
			"filing arbitrary text under a content-addressed id breaks every other guarantee"
		);
		// An id that was never a content hash is not judged — we do not define that format.
		e.id = "not-a-hash".into();
		assert!(id_matches_body(&e), "a non-hashed id is left alone");
	}

	#[test]
	fn remote_kern_ids_excludes_local_kerns() {
		let mut g = GraphGnn::new();
		g.register(crate::base_types::Kern::new("remote-netA-k1", &g.root.id));
		g.register(crate::base_types::Kern::new("local-kern", &g.root.id));

		let ids = remote_kern_ids(&g);
		assert!(ids.iter().any(|k| k == "remote-netA-k1"));
		assert!(
			!ids.iter().any(|k| k == "local-kern"),
			"an unauthenticated LWW delta must never reach a local row"
		);
		assert!(
			!ids.iter().any(|k| k == &g.root.id),
			"least of all the root kern"
		);
	}
	#[test]
	fn hottest_local_picks_the_top_n_and_skips_remote_kerns() {
		use crate::base_types::{Entity, Kern};
		let mut g = GraphGnn::new();
		let local = g.root.id.clone();
		{
			let k = g.kerns.get_mut(&local).expect("root");
			for i in 0..10u32 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						heat: i as f32,
						..Default::default()
					},
				);
			}
		}
		let phantom = "remote-netA-k1";
		g.register(Kern::new(phantom, &local));
		if let Some(k) = g.kerns.get_mut(phantom) {
			k.entities.insert(
				"hot-remote".into(),
				Entity {
					id: "hot-remote".into(),
					heat: 999.0,
					..Default::default()
				},
			);
		}

		let got = hottest_local(&g, 3);

		assert_eq!(
			got.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
			vec!["e9", "e8", "e7"],
			"hottest first"
		);
		assert!(
			!got.iter().any(|e| e.id == "hot-remote"),
			"a peer's row must never be gossiped back out as ours, however hot"
		);
	}

	#[test]
	fn hottest_local_returns_everything_when_under_the_batch() {
		use crate::base_types::Entity;
		let mut g = GraphGnn::new();
		let local = g.root.id.clone();
		{
			let k = g.kerns.get_mut(&local).expect("root");
			for i in 0..2u32 {
				k.entities.insert(
					format!("e{i}"),
					Entity {
						id: format!("e{i}"),
						heat: i as f32,
						..Default::default()
					},
				);
			}
		}
		// n - 1 would underflow select_nth if the guard were missing.
		assert_eq!(hottest_local(&g, 32).len(), 2);
		assert!(hottest_local(&GraphGnn::new(), 32).is_empty());
		assert!(
			hottest_local(&g, 0).is_empty(),
			"a zero batch must return nothing, not underflow select_nth"
		);
	}
	#[test]
	fn a_flood_of_questions_from_one_peer_is_refused() {
		use crate::base_types::Entity;
		use crate::gossip_rate::GOSSIP_QUESTION_PER_MIN;
		let mut graph = GraphGnn::new();
		let root = graph.root.id.clone();
		{
			let k = graph.kerns.get_mut(&root).expect("root");
			let mut e = Entity {
				id: "held".into(),
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			};
			e.gnn_vector = e.vector.clone();
			k.entities.insert("held".into(), e);
		}
		graph.index_entity("held", &root);
		graph.rebuild_index();
		let g = Arc::new(RwLock::new(graph));
		let d = mk_deps(g.clone());

		let probe = |i: usize| GossipMessage {
			kind: GossipKind::Question,
			id: format!("q{i}"),
			origin: "prober:1".into(),
			payload: GossipPayload::Question(QuestionPayload {
				reason_id: format!("r{i}"),
				reason_vec: vec![1.0, 0.0],
				question_text: String::new(),
			}),
		};

		let before = d.node.question_rate.refused();
		// One keypair, many origin strings: the budget must follow the verified
		// PeerId, so the flood exhausts a single budget no matter what `origin`
		// claims.
		for i in 0..(GOSSIP_QUESTION_PER_MIN as usize + 20) {
			handle_question(&d, [9u8; 32], probe(i));
		}
		assert!(
			d.node.question_rate.refused() > before,
			"an unbounded membership oracle is extractable in bulk"
		);
	}

	// ---- Subscription tree e2e (FEDERATION_PLAN §9 gate 4): three nodes
	// chained over real sockets; a leaf publish reaches all three; a healed
	// partition converges within one anti-entropy pass. ----

	fn open_contract() -> (
		crate::gossip_contract::ContractId,
		crate::gossip_contract::ParamsV0,
	) {
		use crate::gossip_contract::*;
		let params = ParamsV0 {
			owners: Vec::new(),
			writers: WritePolicy::Open,
			kinds: None,
			max_entities: 100,
			retention_secs: None,
			private: None,
		};
		(contract_id(SIGNED_CRDT_V0_TAG, &params), params)
	}

	async fn fed_node(
		cid: crate::gossip_contract::ContractId,
		params: crate::gossip_contract::ParamsV0,
	) -> (Arc<Deps>, String) {
		let node = Node::new("127.0.0.1:0", "fed-e2e", vec![]);
		node.enable_ring();
		let mut contracts = std::collections::HashMap::new();
		contracts.insert(
			cid,
			Arc::new(ContractHost {
				params,
				state: RwLock::new(Default::default()),
			}),
		);
		let d = Arc::new(Deps {
			graph: Arc::new(RwLock::new(GraphGnn::new())),
			node: node.clone(),
			queue: None,
			save: None,
			contracts: Arc::new(RwLock::new(contracts)),
			subs: Arc::new(crate::gossip_subs::SubTable::new()),
		});
		node.set_handler(new_handler(d.clone()));
		let addr = node.listen().await.expect("fed node binds");
		(d, addr)
	}

	fn fed_entity(text: &str) -> Entity {
		Entity {
			id: crate::util::content_hash(text),
			statements: vec![text.to_string()],
			chunks: vec![crate::base_types::ChunkPart {
				kind: crate::base_types::ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		}
	}

	fn holds(d: &Arc<Deps>, cid: &crate::gossip_contract::ContractId, id: &str) -> bool {
		let kid = crate::gossip_contract::contract_kern_id(cid);
		let g = d.graph.read();
		g.kerns
			.get(&kid)
			.map(|k| k.entities.contains_key(id))
			.unwrap_or(false)
	}

	async fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
		while !cond() {
			assert!(std::time::Instant::now() < deadline, "timed out: {what}");
			tokio::time::sleep(std::time::Duration::from_millis(20)).await;
		}
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn a_leaf_publish_reaches_all_three_daemons_and_anti_entropy_heals_a_partition() {
		let (cid, params) = open_contract();
		let (a, a_addr) = fed_node(cid, params.clone()).await;
		let (b, b_addr) = fed_node(cid, params.clone()).await;
		let (c, _c_addr) = fed_node(cid, params).await;

		// Chain: c subscribes via b, b via a. Subscribe frames travel over the
		// real sockets; SubAck sets each hop's upstream.
		b.node.send_to(
			&a_addr,
			GossipMessage {
				kind: GossipKind::Subscribe,
				id: "sub-b".into(),
				origin: b.node.addr(),
				payload: GossipPayload::Subscribe(SubscribePayload { contract: cid }),
			},
		);
		c.node.send_to(
			&b_addr,
			GossipMessage {
				kind: GossipKind::Subscribe,
				id: "sub-c".into(),
				origin: c.node.addr(),
				payload: GossipPayload::Subscribe(SubscribePayload { contract: cid }),
			},
		);
		wait_for(
			|| b.subs.upstream(&cid).as_deref() == Some(a_addr.as_str()),
			"b adopts a as its tree parent",
		)
		.await;
		wait_for(
			|| c.subs.upstream(&cid).as_deref() == Some(b_addr.as_str()),
			"c adopts b as its tree parent",
		)
		.await;

		// Ingest at the leaf: the signed delta floods up the tree.
		let leaf_fact = fed_entity("the leaf daemon learned this first");
		let leaf_id = leaf_fact.id.clone();
		publish_to_contract(&c, &cid, leaf_fact).expect("open contract admits the leaf");
		wait_for(
			|| holds(&a, &cid, &leaf_id) && holds(&b, &cid, &leaf_id) && holds(&c, &cid, &leaf_id),
			"leaf publish reaches all three daemons",
		)
		.await;

		// Partition c: sever its tree links so a live delta misses it.
		c.subs.remove(&cid);
		b.subs.remove(&cid);
		let root_fact = fed_entity("published while the leaf was partitioned");
		let root_id = root_fact.id.clone();
		publish_to_contract(&a, &cid, root_fact).expect("root publish");
		wait_for(|| holds(&a, &cid, &root_id), "root holds its own publish").await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
		assert!(
			!holds(&c, &cid, &root_id),
			"partitioned leaf must have missed the live delta"
		);

		// Heal: re-link the chain and run one anti-entropy pass at each hop —
		// summaries flow up, diffs flow back down.
		b.subs.set_upstream(&cid, Some(a_addr.clone()));
		c.subs.set_upstream(&cid, Some(b_addr.clone()));
		contract_sync_once(&b);
		wait_for(|| holds(&b, &cid, &root_id), "b converges via anti-entropy").await;
		contract_sync_once(&c);
		wait_for(
			|| holds(&c, &cid, &root_id),
			"healed leaf converges within one sync pass",
		)
		.await;

		a.node.close();
		b.node.close();
		c.node.close();
	}
}
