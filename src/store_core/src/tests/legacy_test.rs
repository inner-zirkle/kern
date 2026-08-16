//! The legacy decoders, pinned against bytes laid out the way the old builds
//! laid them out.
//!
//! The mirrors below are `Serialize` twins of the frozen `Deserialize` types —
//! the only way to produce v10 bytes from a build that can no longer write
//! them. A test that round-tripped the frozen types against themselves would
//! prove nothing; these encode the OLD field order and assert the CURRENT types
//! come back with every field in the right place. The fields that matter most
//! are the ones *after* the removed one: those are what shift when a layout
//! snapshot is off by one.

use super::*;
use serde::Serialize;

#[derive(Serialize)]
struct EntityTrustTierMirror {
	id: String,
	root_id: String,
	external_id: String,
	superseded_by: String,
	kind: EntityKind,
	status: EntityStatus,
	review: ReviewState,
	trust_tier: u8,
	statements: Vec<String>,
	chunks: Vec<ChunkPart>,
	vector: Embedding,
	gnn_vector: Embedding,
	score: f64,
	conf_alpha: f32,
	conf_beta: f32,
	source: Source,
	created_at: Option<SystemTime>,
	access_count: GCounter,
	accessed_at: Option<SystemTime>,
	heat: f32,
	heat_updated_at: Option<SystemTime>,
	updated_at: Option<SystemTime>,
	valid_until: Option<SystemTime>,
	valid_until_lamport: u64,
	valid_until_producer: String,
	producer_id: String,
	unlinked_count: i32,
	dirty: bool,
	user_id: Option<String>,
	agent_id: Option<String>,
	session_id: Option<String>,
}

impl EntityTrustTierMirror {
	// Distinct values in every field after `trust_tier`, so a one-slot shift
	// cannot pass by coincidence.
	fn sample() -> Self {
		EntityTrustTierMirror {
			id: "e1".into(),
			root_id: "root".into(),
			external_id: "ext-7".into(),
			superseded_by: String::new(),
			kind: EntityKind::Claim,
			status: EntityStatus::Active,
			review: ReviewState::Pending,
			trust_tier: 1, // Inferred
			statements: vec!["the deploy runs on jenkins".into()],
			chunks: Vec::new(),
			vector: Embedding::default(),
			gnn_vector: Embedding::default(),
			score: 0.75,
			conf_alpha: 2.0,
			conf_beta: 3.0,
			source: Source::default(),
			created_at: None,
			access_count: GCounter::default(),
			accessed_at: None,
			heat: 0.5,
			heat_updated_at: None,
			updated_at: None,
			valid_until: None,
			valid_until_lamport: 9,
			valid_until_producer: "prod-a".into(),
			producer_id: "prod-b".into(),
			unlinked_count: 4,
			dirty: true,
			user_id: Some("u1".into()),
			agent_id: None,
			session_id: Some("s1".into()),
		}
	}
}

#[derive(Serialize)]
struct ReasonV10Mirror {
	id: String,
	from: String,
	to: String,
	to_kern_id: String,
	to_net_id: String,
	kind: ReasonKind,
	text: String,
	vector: Embedding,
	score: f64,
	score_lamport: u64,
	score_producer: String,
	traversal_count: GCounter,
	producer_id: String,
	dirty: bool,
}

impl ReasonV10Mirror {
	fn sample() -> Self {
		ReasonV10Mirror {
			id: "r1".into(),
			from: "e1".into(),
			to: "e2".into(),
			to_kern_id: "k9".into(),
			to_net_id: "peer-that-no-longer-exists".into(),
			kind: ReasonKind::Ratification,
			text: "because the pipeline says so".into(),
			vector: Embedding::default(),
			score: 0.875,
			score_lamport: 11,
			score_producer: "sp".into(),
			traversal_count: GCounter::default(),
			producer_id: "pid".into(),
			dirty: false,
		}
	}
}

#[derive(Serialize)]
struct KernV10Mirror<E> {
	id: String,
	root_id: String,
	graviton_text: String,
	graviton_vec: Vec<f32>,
	inner_radius: f64,
	outer_radius: f64,
	spawn_reason_id: String,
	parent: String,
	children: Vec<String>,
	entities: HashMap<String, E>,
	refs: HashMap<String, EntityRef>,
	reasons: HashMap<String, ReasonV10Mirror>,
	by_from: HashMap<String, Vec<String>>,
	by_to: HashMap<String, Vec<String>>,
	source_index: HashMap<String, String>,
	claim_kinds: HashMap<String, String>,
	claim_kind_parents: HashMap<String, String>,
	gnn_weights: Vec<u8>,
	mass: f64,
}

impl<E> KernV10Mirror<E> {
	fn sample(entities: HashMap<String, E>) -> Self {
		let mut reasons = HashMap::new();
		reasons.insert("r1".to_string(), ReasonV10Mirror::sample());
		KernV10Mirror {
			id: "k1".into(),
			root_id: "root".into(),
			graviton_text: "infrastructure".into(),
			graviton_vec: vec![0.25, 0.5],
			inner_radius: 1.5,
			outer_radius: 2.5,
			spawn_reason_id: "sr".into(),
			parent: "root".into(),
			children: vec!["c1".into()],
			entities,
			refs: HashMap::new(),
			reasons,
			by_from: HashMap::new(),
			by_to: HashMap::new(),
			source_index: HashMap::new(),
			claim_kinds: HashMap::new(),
			claim_kind_parents: HashMap::new(),
			gnn_weights: vec![1, 2, 3],
			mass: 3.5,
		}
	}
}

#[derive(Serialize)]
struct StoredKernV10Mirror<E> {
	kern: KernV10Mirror<E>,
	entity_vecs: HashMap<String, StoredVec>,
	reason_vecs: HashMap<String, StoredVec>,
	temporal: HashMap<String, StoredTemporal>,
}

fn encode_mirror<E: Serialize>(kern: KernV10Mirror<E>) -> Vec<u8> {
	let row = StoredKernV10Mirror {
		kern,
		entity_vecs: HashMap::new(),
		reason_vecs: HashMap::new(),
		temporal: HashMap::new(),
	};
	bincode::serde::encode_to_vec(&row, crate::bincode_cfg()).expect("encode v10 mirror")
}

#[test]
fn the_post_trust_tier_v10_layout_decodes_with_every_later_field_in_place() {
	let mut entities = HashMap::new();
	entities.insert("e1".to_string(), EntityTrustTierMirror::sample());
	let raw = encode_mirror(KernV10Mirror::sample(entities));

	let row = decode_v10_kern(&raw).expect("a v10 row must decode");
	let e = row.kern.entities.get("e1").expect("the entity survived");

	// Everything after the dropped `trust_tier` slot: if the snapshot were off
	// by one, these would be shifted or the decode would have failed outright.
	assert_eq!(e.statements, vec!["the deploy runs on jenkins".to_string()]);
	assert_eq!(e.score, 0.75);
	assert_eq!(e.conf_alpha, 2.0);
	assert_eq!(e.conf_beta, 3.0);
	assert_eq!(e.heat, 0.5);
	assert_eq!(e.valid_until_lamport, 9);
	assert_eq!(e.valid_until_producer, "prod-a");
	assert_eq!(e.producer_id, "prod-b");
	assert_eq!(e.unlinked_count, 4);
	assert!(e.dirty);
	assert_eq!(e.user_id.as_deref(), Some("u1"));
	assert_eq!(e.session_id.as_deref(), Some("s1"));
	// And the fields before it, which no shift would disturb but a wrong type would.
	assert_eq!(e.id, "e1");
	assert_eq!(e.external_id, "ext-7");
	assert_eq!(e.review, ReviewState::Pending);

	let r = row.kern.reasons.get("r1").expect("the edge survived");
	// The same proof one field deeper: everything after the dropped `to_net_id`.
	assert_eq!(r.kind, ReasonKind::Ratification);
	assert_eq!(r.text, "because the pipeline says so");
	assert_eq!(r.score, 0.875);
	assert_eq!(r.score_lamport, 11);
	assert_eq!(r.score_producer, "sp");
	assert_eq!(r.producer_id, "pid");
	assert_eq!(r.from, "e1");
	assert_eq!(r.to_kern_id, "k9");

	assert_eq!(row.kern.graviton_text, "infrastructure");
	assert_eq!(row.kern.mass, 3.5);
	assert_eq!(row.kern.gnn_weights, vec![1, 2, 3]);
}

#[test]
fn the_pre_trust_tier_v10_layout_decodes_under_the_same_version_byte() {
	// The ambiguity f60fbce created: this row calls itself v10 too, and has no
	// `trust_tier`. An entity without it is byte-identical to the current type.
	let mut entities: HashMap<String, Entity> = HashMap::new();
	let mut e = Entity {
		id: "e1".into(),
		producer_id: "prod-b".into(),
		unlinked_count: 4,
		..Default::default()
	};
	e.statements = vec!["older row, same version byte".into()];
	e.score = 0.5;
	entities.insert("e1".to_string(), e);
	let raw = encode_mirror(KernV10Mirror::sample(entities));

	let row = decode_v10_kern(&raw).expect("the pre-trust_tier row must decode too");
	let e = row.kern.entities.get("e1").expect("the entity survived");
	assert_eq!(
		e.statements,
		vec!["older row, same version byte".to_string()]
	);
	assert_eq!(e.score, 0.5);
	assert_eq!(e.producer_id, "prod-b");
	assert_eq!(e.unlinked_count, 4);
	assert_eq!(
		row.kern.reasons.get("r1").expect("the edge survived").text,
		"because the pipeline says so"
	);
}

#[test]
fn bytes_that_are_neither_layout_are_refused_rather_than_guessed_at() {
	// Zeros are the case worth pinning: bincode reads them as empty strings,
	// empty maps and zeroed numbers, so they DECODE — into a kern that never
	// existed, with no id. A decoder that trusted "it parsed" would migrate
	// garbage into the store under an empty key.
	assert!(
		decode_v10_kern(&[0u8; 64]).is_none(),
		"a body that parses but carries no kern id must not be accepted"
	);
	assert!(decode_v10_kern(&[]).is_none(), "empty body");
	assert!(
		decode_v10_kern(&[0xff; 128]).is_none(),
		"a body of invalid tags and lengths must not be forced into a layout"
	);
}
