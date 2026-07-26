use serde::{Deserialize, Serialize};

// serde/bincode encode the declaration index, so variant ORDER is on-wire;
// reordering is a breaking wire change (alpha: peers upgrade together).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum GossipKind {
	Sphere = 0,
	Question = 1,
	Pulse = 2,
	PeerExchange = 3,
	Fetch = 4,
	Delta = 5,
	EntitySync = 6,
	// Ring routing (FEDERATION_PLAN §2): request/response like Fetch.
	FindNearest = 7,
	Nearest = 8,
	// Contract kerns (FEDERATION_PLAN §3/§4).
	Subscribe = 9,
	SubAck = 10,
	ContractDelta = 11,
	SyncSummary = 12,
	SyncDiff = 13,
	Tombstone = 14,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipMessage {
	pub kind: GossipKind,
	pub id: String,
	pub origin: String,
	pub payload: GossipPayload,
}

// The signed wire envelope every TCP frame travels in. `pubkey` rides along
// because PeerId is its blake3 hash — the receiver cannot verify against a
// hash alone. The signature covers blake3(body || lamport_le); see
// `identity::frame_digest`. Verification happens before ANY per-peer state
// is touched (seen-set, peer list, rate limits): an invalid signature is
// free to send and must buy nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedFrame {
	pub pubkey: [u8; 32],
	pub sig: Vec<u8>,
	pub lamport: u64,
	pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipPayload {
	Sphere(SpherePayload),
	Question(QuestionPayload),
	Pulse(PulsePayload),
	PeerExchange(PeerExchangePayload),
	FetchRequest(FetchPayload),
	FetchResult(FetchResultPayload),
	CrdtDelta(CrdtDeltaPayload),
	EntitySync(EntitySyncPayload),
	FindNearest(FindNearestPayload),
	Nearest(NearestPayload),
	Subscribe(SubscribePayload),
	SubAck(SubAckPayload),
	ContractDelta(ContractDeltaPayload),
	SyncSummary(SyncSummaryPayload),
	SyncDiff(SyncDiffPayload),
	Tombstone(TombstonePayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribePayload {
	pub contract: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAckPayload {
	pub contract: [u8; 32],
	pub summary: crate::gossip::contract::Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractDeltaPayload {
	pub contract: [u8; 32],
	pub delta: crate::gossip::contract::Delta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSummaryPayload {
	pub contract: [u8; 32],
	pub summary: crate::gossip::contract::Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncDiffPayload {
	pub contract: [u8; 32],
	pub delta: crate::gossip::contract::Delta,
}

// A signed forward pointer published in the OLD contract when its params
// are amended: the key is the policy hash, so a policy change moves the key
// and subscribers follow the pointer once (FEDERATION_PLAN §5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TombstonePayload {
	pub contract: [u8; 32],
	pub new_id: [u8; 32],
	pub sig: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindNearestPayload {
	pub target: f64,
}

// The responder's own entry plus its `near` set — everything a joiner needs
// to keep hopping greedily or to adopt a neighborhood at the terminal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearestPayload {
	pub peers: Vec<crate::gossip::ring::PeerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpherePayload {
	pub network_id: String,
	pub kern_id: String,
	pub graviton_vec: Vec<f32>,
	pub graviton_text: String,
	pub entity_id: String,
	// Cosine distance (1 - cos), smaller = closer.
	pub inner_radius: f64,
	// Invariant: inner_radius <= outer_radius.
	pub outer_radius: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionPayload {
	pub reason_id: String,
	pub reason_vec: Vec<f32>,
	pub question_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PulsePayload {
	pub kern_id: String,
	pub strength: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerExchangePayload {
	pub peers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchPayload {
	pub resource: String,
	pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchResultPayload {
	pub found: bool,
	pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CrdtTarget {
	ThoughtAccessCount = 0,
	ReasonTraversalCount = 1,
	ReasonScore = 2,
	ValidUntil = 3,
	Statements = 4,
}

impl CrdtTarget {
	pub fn from_u8(v: u8) -> Option<Self> {
		match v {
			0 => Some(Self::ThoughtAccessCount),
			1 => Some(Self::ReasonTraversalCount),
			2 => Some(Self::ReasonScore),
			3 => Some(Self::ValidUntil),
			4 => Some(Self::Statements),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySyncPayload {
	pub network_id: String,
	pub kern_id: String,
	pub entities: Vec<crate::base::types::Entity>,
}

// value is the sender's ABSOLUTE replica-slot total, not an increment — a
// delta-since-last would be lost under the receiver's max-merge.
// lamport + producer carry the LWW-Register tiebreak for ReasonScore / ValidUntil.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtDeltaPayload {
	pub kern_id: String,
	pub object_id: String,
	pub target: CrdtTarget,
	pub replica: String,
	pub value: u64,
	pub lamport: u64,
	pub producer: String,
	// Encoded LWW value for ReasonScore / ValidUntil (bincode of the f64 / Option<SystemTime>).
	pub lww_value: Vec<u8>,
	// Encoded OR-Set delta for Statements (bincode of Vec<String> adds).
	pub orset_delta: Vec<u8>,
}
