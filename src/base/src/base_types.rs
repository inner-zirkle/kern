//! The domain types the whole crate shares: [`Entity`] (a thought, with its
//! bitemporal validity window and CRDT counters), [`Reason`] (a typed edge),
//! [`Kern`] (one cluster of entities plus its indices), and the kind/status/
//! source enums. Serialized with bincode into the store, so field changes here
//! are format changes — see the alpha policy in `base_store`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

use crate::crdt::GCounter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ChunkPartKind {
	Context = 0,
	StatementRef = 1,
}

// `Receipt` is not a kind (receipts live in the journal); `Superseded` is not a
// kind — lifecycle moved to EntityStatus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum EntityKind {
	// GC-immune while Active — Facts are never auto-forgotten.
	Fact = 0,
	#[default]
	Claim = 1,
	Document = 2,
	Question = 3,
	Conclusion = 4,
}

impl EntityKind {
	// Stable labels — the MCP query `kind` filter matches these strings.
	pub fn as_str(self) -> &'static str {
		match self {
			EntityKind::Fact => "fact",
			EntityKind::Claim => "claim",
			EntityKind::Document => "document",
			EntityKind::Question => "question",
			EntityKind::Conclusion => "conclusion",
		}
	}

	pub fn parse(s: &str) -> Option<Self> {
		match s {
			"fact" => Some(EntityKind::Fact),
			"claim" => Some(EntityKind::Claim),
			"document" => Some(EntityKind::Document),
			"question" => Some(EntityKind::Question),
			"conclusion" => Some(EntityKind::Conclusion),
			_ => None,
		}
	}

	// The inverse of the `as u8` the MCP payload carries: a reader decoding a
	// daemon's answer has the discriminant, not the variant, and maps it back
	// here rather than duplicating the numbering.
	pub fn from_u8(n: u8) -> Option<Self> {
		match n {
			0 => Some(EntityKind::Fact),
			1 => Some(EntityKind::Claim),
			2 => Some(EntityKind::Document),
			3 => Some(EntityKind::Question),
			4 => Some(EntityKind::Conclusion),
			_ => None,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum EntityStatus {
	#[default]
	Active = 0,
	Superseded = 1,
}

/// Curation state. `Active` is the default so a schema addition is not a
/// behaviour change: a claim is retrievable unless a host's review policy asked
/// for it to be held (`IngestConfig::review_policy`), and only an
/// `exclude_pending` query drops a held one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(i32)]
pub enum ReviewState {
	#[default]
	Active = 0,
	Pending = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[repr(i32)]
pub enum ReasonKind {
	#[default]
	Similarity = 0,
	Provenance = 1,
	Question = 2,
	Spawn = 3,
	Supersedes = 4,
	Ratification = 5,
	Rephrase = 6,
}

impl ReasonKind {
	// The inverse of the `as i32` the MCP edge payload carries.
	pub fn from_i32(n: i32) -> Option<Self> {
		match n {
			0 => Some(ReasonKind::Similarity),
			1 => Some(ReasonKind::Provenance),
			2 => Some(ReasonKind::Question),
			3 => Some(ReasonKind::Spawn),
			4 => Some(ReasonKind::Supersedes),
			5 => Some(ReasonKind::Ratification),
			6 => Some(ReasonKind::Rephrase),
			_ => None,
		}
	}

	pub fn fallback_label(self) -> Option<&'static str> {
		match self {
			ReasonKind::Supersedes => Some("superseded by a newer version"),
			ReasonKind::Rephrase => Some("rephrased as"),
			_ => None,
		}
	}
}

// URI schemes: file://<path>, ticket://<system>/<id>[#section],
// session://<id>[#slice], agent://<name>, inline://<hash>.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Source {
	File {
		path: String,
		section: String,
		title: String,
		author: String,
		url: String,
	},
	Ticket {
		system: String,
		object_id: String,
		section: String,
		title: String,
		author: String,
		url: String,
	},
	Session {
		session_id: String,
		section: String,
		title: String,
	},
	Agent {
		agent: String,
		object_id: String,
		title: String,
	},
	Inline {
		hash: String,
		section: String,
	},
}

impl Default for Source {
	fn default() -> Self {
		Source::Inline {
			hash: String::new(),
			section: String::new(),
		}
	}
}

impl Source {
	// Stable tag — the MCP query `scheme` filter matches on it.
	pub fn scheme(&self) -> &'static str {
		match self {
			Source::File { .. } => "file",
			Source::Ticket { .. } => "ticket",
			Source::Session { .. } => "session",
			Source::Agent { .. } => "agent",
			Source::Inline { .. } => "inline",
		}
	}

	pub fn parse_scheme(s: &str) -> Option<&'static str> {
		match s {
			"file" => Some("file"),
			"ticket" => Some("ticket"),
			"session" => Some("session"),
			"agent" => Some("agent"),
			"inline" => Some("inline"),
			_ => None,
		}
	}

	pub fn object_id(&self) -> &str {
		match self {
			Source::File { path, .. } => path,
			Source::Ticket { object_id, .. } => object_id,
			Source::Session { session_id, .. } => session_id,
			Source::Agent { object_id, .. } => object_id,
			Source::Inline { hash, .. } => hash,
		}
	}

	pub fn section(&self) -> &str {
		match self {
			Source::File { section, .. } => section,
			Source::Ticket { section, .. } => section,
			Source::Session { section, .. } => section,
			Source::Agent { .. } => "",
			Source::Inline { section, .. } => section,
		}
	}

	pub fn title(&self) -> &str {
		match self {
			Source::File { title, .. }
			| Source::Ticket { title, .. }
			| Source::Session { title, .. }
			| Source::Agent { title, .. } => title,
			Source::Inline { .. } => "",
		}
	}

	pub fn author(&self) -> &str {
		match self {
			Source::File { author, .. } | Source::Ticket { author, .. } => author,
			_ => "",
		}
	}

	pub fn url(&self) -> &str {
		match self {
			Source::File { url, .. } | Source::Ticket { url, .. } => url,
			_ => "",
		}
	}

	pub fn system(&self) -> &str {
		match self {
			Source::Ticket { system, .. } => system,
			Source::File { .. } => "file",
			Source::Session { .. } => "session",
			Source::Agent { agent, .. } => agent,
			Source::Inline { .. } => "inline",
		}
	}

	// Stable content-addressed id; changing the hashed layout breaks existing ids.
	pub fn source_id(&self) -> Option<String> {
		let scheme = self.scheme();
		let object = self.object_id();
		if object.is_empty() {
			return None;
		}
		Some(util::content_hash(&format!(
			"{}\x00{}\x00{}",
			scheme,
			object,
			self.section()
		)))
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkPart {
	pub kind: ChunkPartKind,
	pub text: String,
	pub index: usize,
}

/// Multi-tenancy scoping dimensions. None on each field = global (unscoped).
/// Carried on Entity for query filtering; threaded through ingest via this
/// helper to avoid bloating every function signature.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scoping {
	pub user_id: Option<String>,
	pub agent_id: Option<String>,
	pub session_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entity {
	pub id: String,
	pub root_id: String,
	pub external_id: String,
	pub superseded_by: String,
	pub kind: EntityKind,
	pub status: EntityStatus,
	pub review: ReviewState,
	pub statements: Vec<String>,
	pub chunks: Vec<ChunkPart>,
	pub vector: Embedding,
	pub gnn_vector: Embedding,
	pub score: f64,
	pub conf_alpha: f32,
	pub conf_beta: f32,
	pub source: Source,
	pub created_at: Option<SystemTime>,
	pub access_count: GCounter,
	pub accessed_at: Option<SystemTime>,
	pub heat: f32,
	pub heat_updated_at: Option<SystemTime>,
	pub updated_at: Option<SystemTime>,
	pub valid_until: Option<SystemTime>,
	pub valid_until_lamport: u64,
	pub valid_until_producer: String,
	pub producer_id: String,
	pub unlinked_count: i32,
	pub dirty: bool,
	// Multi-tenancy scoping. None = global (unscoped). Optional on ingest,
	// filterable on query. Backward-compatible: absent in stored data deser
	// as None.
	pub user_id: Option<String>,
	pub agent_id: Option<String>,
	pub session_id: Option<String>,
	// serde(skip) is load-bearing: StoredKern's side-map persists these, never the
	// embedded entity bytes. valid_from/valid_to = world time, invalidated_at =
	// transaction time.
	#[serde(skip)]
	pub valid_from: Option<SystemTime>,
	#[serde(skip)]
	pub valid_to: Option<SystemTime>,
	#[serde(skip)]
	pub invalidated_at: Option<SystemTime>,
}

impl Entity {
	pub fn text(&self) -> String {
		let mut buf = String::new();
		for c in &self.chunks {
			match c.kind {
				ChunkPartKind::Context => buf.push_str(&c.text),
				ChunkPartKind::StatementRef => {
					if c.index < self.statements.len() {
						buf.push_str(&self.statements[c.index]);
					}
				}
			}
		}
		buf
	}

	// Collapses to a single Context chunk and drops the original statement refs.
	pub fn set_text(&mut self, text: String) {
		self.statements.clear();
		self.chunks = vec![ChunkPart {
			kind: ChunkPartKind::Context,
			text,
			index: 0,
		}];
		self.updated_at = Some(SystemTime::now());
		self.dirty = true;
	}

	pub fn is_fact(&self) -> bool {
		self.kind == EntityKind::Fact
	}

	pub fn is_superseded(&self) -> bool {
		self.status == EntityStatus::Superseded
	}

	pub fn valid_from_or_created(&self) -> Option<SystemTime> {
		self.valid_from.or(self.created_at)
	}

	// Half-open [valid_from, valid_to): unknown lower bound never excludes.
	pub fn is_valid_at(&self, instant: SystemTime) -> bool {
		if let Some(from) = self.valid_from_or_created() {
			if instant < from {
				return false;
			}
		}
		if let Some(to) = self.valid_to {
			if instant >= to {
				return false;
			}
		}
		true
	}

	// Stamps the clocks only — caller still owns status/superseded_by and ANN eviction.
	pub fn stamp_invalidated(&mut self, at: SystemTime, valid_to: SystemTime) {
		self.invalidated_at = Some(at);
		if self.valid_to.is_none() {
			self.valid_to = Some(valid_to);
		}
	}

	pub fn has_vector(&self) -> bool {
		!self.vector.is_empty()
	}

	pub fn has_gnn_vector(&self) -> bool {
		!self.gnn_vector.is_empty()
	}

	pub fn conf_mean(&self) -> f64 {
		let a = self.conf_alpha as f64;
		let b = self.conf_beta as f64;
		let n = a + b;
		if n <= 0.0 {
			return 0.5;
		}
		a / n
	}

	pub fn conf_variance(&self) -> f64 {
		let a = self.conf_alpha as f64;
		let b = self.conf_beta as f64;
		let n = a + b;
		if n <= 0.0 {
			return 0.0;
		}
		(a * b) / (n * n * (n + 1.0))
	}

	pub fn refresh_score(&mut self) {
		self.score = self.conf_mean();
	}

	pub fn observe_support(&mut self, w: f64) {
		let w = w.clamp(0.0, 1.0) as f32;
		self.conf_alpha += w;
		self.updated_at = Some(SystemTime::now());
		self.refresh_score();
	}

	pub fn observe_contradict(&mut self, w: f64) {
		let w = w.clamp(0.0, 1.0) as f32;
		self.conf_beta += w;
		self.updated_at = Some(SystemTime::now());
		self.refresh_score();
	}
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reason {
	pub id: String,
	pub from: String,
	pub to: String,
	pub to_kern_id: String,
	pub kind: ReasonKind,
	pub text: String,
	pub vector: Embedding,
	pub score: f64,
	pub score_lamport: u64,
	pub score_producer: String,
	pub traversal_count: GCounter,
	pub producer_id: String,
	pub dirty: bool,
}

impl Reason {
	pub fn set_text(&mut self, text: String) {
		self.text = text;
		self.dirty = true;
	}

	pub fn has_vector(&self) -> bool {
		!self.vector.is_empty()
	}

	pub fn is_enriched(&self) -> bool {
		!self.text.is_empty()
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRef {
	pub kern_id: String,
	pub entity_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Kern {
	pub id: String,
	pub root_id: String,
	pub graviton_text: String,
	pub graviton_vec: Vec<f32>,
	pub inner_radius: f64,
	pub outer_radius: f64,
	pub spawn_reason_id: String,
	pub parent: String,
	pub children: Vec<String>,

	pub entities: HashMap<String, Entity>,
	pub refs: HashMap<String, EntityRef>,
	pub reasons: HashMap<String, Reason>,
	pub by_from: HashMap<String, Vec<String>>,
	pub by_to: HashMap<String, Vec<String>>,
	pub source_index: HashMap<String, String>,
	pub claim_kinds: HashMap<String, String>,
	// RDFS-lite `subClassOf` over claim kinds: `claim_kind_parents[child] = parent`.
	// A query filtering on a parent kind also admits its transitive children
	// (see `claim_kind_closure`). Validation is closed-world — unknown parents
	// and cycles are refused at registration, never inferred around.
	pub claim_kind_parents: HashMap<String, String>,

	pub gnn_weights: Vec<u8>,

	pub mass: f64,

	#[serde(skip)]
	pub last_access: Option<SystemTime>,
}

// The only non-deterministic input to kern-id derivation.
use util::now_nanos;

fn unnamed_kern_id(parent_id: &str, nonce_nanos: u128) -> String {
	util::content_hash(&format!("{parent_id}{nonce_nanos}"))
}

// Name folded into the hash so two gravitons under one parent never collide.
fn named_child_kern_id(parent_id: &str, name: &str, nonce_nanos: u128) -> String {
	util::content_hash(&format!("{parent_id}{name}{nonce_nanos}"))
}

impl Kern {
	pub fn new(id: impl Into<String>, parent_id: impl Into<String>) -> Self {
		Self {
			id: id.into(),
			parent: parent_id.into(),
			last_access: Some(SystemTime::now()),
			..Self::empty()
		}
	}

	pub fn new_root() -> Self {
		let mut k = Self::new("root", "");
		k.last_access = Some(SystemTime::now());
		k
	}

	pub fn new_unnamed(parent_id: &str, root_id: &str) -> Self {
		let mut k = Self::new(unnamed_kern_id(parent_id, now_nanos()), parent_id);
		k.root_id = root_id.to_string();
		k
	}

	// Empty vec (the generic catch-all) never matches similarity routing.
	pub fn new_named_child(parent_id: &str, root_id: &str, name: &str, vec: Vec<f32>) -> Self {
		let mut k = Self::new(named_child_kern_id(parent_id, name, now_nanos()), parent_id);
		k.root_id = root_id.to_string();
		k.graviton_text = name.to_string();
		k.graviton_vec = vec;
		k.inner_radius = crate::base_constants::KERN_INNER_RADIUS;
		k.outer_radius = crate::base_constants::KERN_OUTER_RADIUS;
		k
	}

	pub fn is_unnamed(&self) -> bool {
		self.graviton_text.is_empty()
	}

	pub fn is_named(&self) -> bool {
		!self.graviton_text.is_empty()
	}

	pub fn has_graviton(&self) -> bool {
		!self.graviton_text.is_empty() && !self.graviton_vec.is_empty()
	}

	// Register a claim kind, optionally as a sub-kind of `parent`. `builtins` is
	// the caller-supplied builtin set (base must not reach into ingest for
	// `DEFAULT_KINDS`). The parent must already exist and the edge must not close
	// a cycle — walking up from `parent` may never reach `name`.
	pub fn add_claim_kind(
		&mut self,
		name: &str,
		description: &str,
		parent: Option<&str>,
		builtins: &[&str],
	) -> Result<(), String> {
		if let Some(p) = parent {
			if p == name {
				return Err(format!("claim kind {name} cannot be its own parent"));
			}
			if !builtins.contains(&p) && !self.claim_kinds.contains_key(p) {
				return Err(format!("unknown parent claim kind: {p}"));
			}
			// Hop cap: a remote merge could have unioned two acyclic maps into a
			// cycle, so the ancestor walk terminates on hops, not on trust.
			let mut cur: &str = p;
			for _ in 0..=self.claim_kind_parents.len() {
				match self.claim_kind_parents.get(cur) {
					Some(next) if next.as_str() == name => {
						return Err(format!(
							"parent {p} would make {name} an ancestor of itself"
						));
					}
					Some(next) => cur = next,
					None => break,
				}
			}
			self
				.claim_kind_parents
				.insert(name.to_string(), p.to_string());
		} else {
			self.claim_kind_parents.remove(name);
		}
		self
			.claim_kinds
			.insert(name.to_string(), description.to_string());
		Ok(())
	}

	pub fn rm_claim_kind(&mut self, name: &str) {
		self.claim_kinds.remove(name);
		self.claim_kind_parents.remove(name);
		// Orphaned children float to the top level rather than pointing at a ghost.
		self.claim_kind_parents.retain(|_, p| p != name);
	}

	/// `label` plus every registered descendant — the transitive `subClassOf`
	/// closure a query filter on `label` admits. `out` doubles as the visited
	/// set, so a cycle a remote merge smuggled in cannot loop this walk.
	pub fn claim_kind_closure(&self, label: &str) -> Vec<String> {
		let mut out = vec![label.to_string()];
		loop {
			let before = out.len();
			for (child, parent) in &self.claim_kind_parents {
				if out.iter().any(|o| o == parent) && !out.iter().any(|o| o == child) {
					out.push(child.clone());
				}
			}
			if out.len() == before {
				return out;
			}
		}
	}

	fn empty() -> Self {
		Self {
			id: String::new(),
			root_id: String::new(),
			graviton_text: String::new(),
			graviton_vec: Vec::new(),
			inner_radius: 0.0,
			outer_radius: 0.0,
			spawn_reason_id: String::new(),
			parent: String::new(),
			children: Vec::new(),
			entities: HashMap::new(),
			refs: HashMap::new(),
			reasons: HashMap::new(),
			by_from: HashMap::new(),
			by_to: HashMap::new(),
			source_index: HashMap::new(),
			claim_kinds: HashMap::new(),
			claim_kind_parents: HashMap::new(),
			gnn_weights: Vec::new(),
			mass: 1.0,
			last_access: None,
		}
	}
}

/// One allocation per embedding, shared between the kern map and whichever
/// vector index holds it — the two copies ROADMAP item 83 removed.
///
/// Sharing is what makes an aliasing bug conceivable, and the type is what
/// rules it out: `Arc<[f32]>` has no `DerefMut`, so a holder can only replace
/// its whole handle. Writing through one is a compile error, not a race:
///
/// ```compile_fail
/// let mut v: kern::base::types::Embedding = vec![1.0f32, 0.0].into();
/// v[0] = 2.0;
/// ```
pub type Embedding = std::sync::Arc<[f32]>;

pub fn mk_entity(id: &str, text: &str, heat: f64, kind: EntityKind) -> Entity {
	let mut e = Entity {
		id: id.to_string(),
		root_id: String::new(),
		external_id: String::new(),
		superseded_by: String::new(),
		kind,
		status: EntityStatus::Active,
		review: ReviewState::default(),
		statements: vec![text.to_string()],
		chunks: vec![ChunkPart {
			kind: ChunkPartKind::StatementRef,
			text: String::new(),
			index: 0,
		}],
		vector: vec![0.0; 8].into(),
		gnn_vector: Vec::new().into(),
		score: 0.0,
		conf_alpha: 2.0,
		conf_beta: 1.0,
		source: Source::Inline {
			hash: id.into(),
			section: String::new(),
		},
		created_at: None,
		access_count: GCounter::new(),
		accessed_at: None,
		heat: heat as f32,
		heat_updated_at: None,
		updated_at: None,
		valid_until: None,
		valid_until_lamport: 0,
		valid_until_producer: String::new(),
		producer_id: String::new(),
		unlinked_count: 0,
		dirty: false,
		user_id: None,
		agent_id: None,
		session_id: None,
		valid_from: None,
		valid_to: None,
		invalidated_at: None,
	};
	e.refresh_score();
	e
}

#[cfg(test)]
#[path = "tests/base_types_test.rs"]
mod base_types_tests;
