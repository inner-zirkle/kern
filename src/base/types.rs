use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::SystemTime;

use super::util;
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
	pub to_net_id: String,
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

	pub fn is_remote(&self) -> bool {
		!self.to_net_id.is_empty()
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
		k.inner_radius = crate::base::constants::KERN_INNER_RADIUS;
		k.outer_radius = crate::base::constants::KERN_OUTER_RADIUS;
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

	pub fn is_remote(&self) -> bool {
		self.id.starts_with("remote-")
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
						return Err(format!("parent {p} would make {name} an ancestor of itself"));
					}
					Some(next) => cur = next,
					None => break,
				}
			}
			self.claim_kind_parents.insert(name.to_string(), p.to_string());
		} else {
			self.claim_kind_parents.remove(name);
		}
		self.claim_kinds.insert(name.to_string(), description.to_string());
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

#[cfg(test)]
pub(crate) fn mk_entity(id: &str, text: &str, heat: f64, kind: EntityKind) -> Entity {
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
mod tests {
	use super::*;

	#[test]
	fn entity_set_text_replaces_text_and_marks_dirty() {
		let mut e = Entity {
			statements: vec!["old statement".into()],
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		};
		assert_eq!(e.text(), "old statement");
		assert!(!e.dirty);

		e.set_text("brand new text".into());

		assert_eq!(e.text(), "brand new text");
		assert!(e.dirty, "edit must mark the entity dirty for reevaluation");
		assert!(
			e.statements.is_empty(),
			"statement refs are dropped on edit"
		);
		assert!(e.updated_at.is_some());
	}

	#[test]
	fn reason_set_text_replaces_text_and_marks_dirty() {
		let mut r = Reason {
			text: "old edge".into(),
			..Default::default()
		};
		assert!(!r.dirty);
		r.set_text("new edge".into());
		assert_eq!(r.text, "new edge");
		assert!(r.dirty);
	}

	#[test]
	fn conf_mean_and_variance_handle_a_zero_total_prior() {
		let e = Entity {
			conf_alpha: 0.0,
			conf_beta: 0.0,
			..Default::default()
		};
		assert_eq!(e.conf_mean(), 0.5);
		assert_eq!(e.conf_variance(), 0.0);
	}

	#[test]
	fn conf_mean_and_variance_for_a_beta_prior() {
		// Beta(2,1): mean = a/(a+b) = 2/3; var = ab / ((a+b)^2 (a+b+1)) = 2/36.
		let e = Entity {
			conf_alpha: 2.0,
			conf_beta: 1.0,
			..Default::default()
		};
		assert!((e.conf_mean() - 2.0 / 3.0).abs() < 1e-12);
		assert!((e.conf_variance() - 2.0 / 36.0).abs() < 1e-12);
	}

	#[test]
	fn kern_has_graviton_requires_both_text_and_vector() {
		let mut k = Kern::new("k", "");
		assert!(!k.has_graviton(), "fresh kern has no graviton");
		k.graviton_text = "topic".into();
		assert!(
			!k.has_graviton(),
			"text without a vector is not a full graviton"
		);
		k.graviton_vec = vec![0.1, 0.2];
		assert!(k.has_graviton(), "text + vector -> gravitationally bound");
	}

	#[test]
	fn new_named_child_sets_graviton_parent_and_root() {
		let k = Kern::new_named_child("parent", "rootid", "generic", vec![0.5, 0.5]);
		assert_eq!(k.parent, "parent");
		assert_eq!(k.root_id, "rootid");
		assert_eq!(k.graviton_text, "generic");
		assert_eq!(k.graviton_vec, vec![0.5, 0.5]);
		assert!(k.is_named() && k.has_graviton());
		assert!(!k.id.is_empty(), "id is the content hash, never empty");
	}

	#[test]
	fn kern_id_derivation_is_deterministic_and_input_sensitive() {
		assert_eq!(unnamed_kern_id("p", 42), unnamed_kern_id("p", 42));
		assert_eq!(
			named_child_kern_id("p", "code", 9),
			named_child_kern_id("p", "code", 9)
		);
		assert_ne!(unnamed_kern_id("p", 1), unnamed_kern_id("p", 2));
		assert_ne!(unnamed_kern_id("a", 7), unnamed_kern_id("b", 7));
		assert_ne!(
			named_child_kern_id("p", "code", 9),
			named_child_kern_id("p", "docs", 9)
		);
		assert!(!unnamed_kern_id("p", 0).is_empty());
		assert!(!named_child_kern_id("p", "x", 0).is_empty());
	}

	#[test]
	fn entity_kind_serde_roundtrip() {
		for k in [
			EntityKind::Fact,
			EntityKind::Claim,
			EntityKind::Document,
			EntityKind::Question,
			EntityKind::Conclusion,
		] {
			let json = serde_json::to_string(&k).expect("serialize");
			let back: EntityKind = serde_json::from_str(&json).expect("deserialize");
			assert_eq!(k, back, "roundtrip failed for {k:?}");
			assert_eq!(EntityKind::parse(k.as_str()), Some(k));
		}
	}

	#[test]
	fn entity_status_default_is_active() {
		assert_eq!(EntityStatus::default(), EntityStatus::Active);
	}

	#[test]
	fn source_scheme_returns_correct_tag() {
		let cases: &[(Source, &str)] = &[
			(
				Source::File {
					path: "/x".into(),
					section: String::new(),
					title: String::new(),
					author: String::new(),
					url: String::new(),
				},
				"file",
			),
			(
				Source::Ticket {
					system: "gh".into(),
					object_id: "1".into(),
					section: String::new(),
					title: String::new(),
					author: String::new(),
					url: String::new(),
				},
				"ticket",
			),
			(
				Source::Session {
					session_id: "s".into(),
					section: String::new(),
					title: String::new(),
				},
				"session",
			),
			(
				Source::Agent {
					agent: "a".into(),
					object_id: "o".into(),
					title: String::new(),
				},
				"agent",
			),
			(
				Source::Inline {
					hash: "h".into(),
					section: String::new(),
				},
				"inline",
			),
		];
		for (src, want) in cases {
			assert_eq!(src.scheme(), *want);
		}
		assert!(Source::parse_scheme("file").is_some());
		assert!(Source::parse_scheme("bogus").is_none());
	}

	#[test]
	fn source_id_pins_null_delimited_composition() {
		// `source_id` = content_hash(scheme \x00 object \x00 section); the \x00
		// delimiter is load-bearing, not a separator of convenience. Changing it
		// or reordering the fields orphans every source id and breaks the gossip
		// import guard that checks `content_hash(&e.text()) == e.id`
		// (ROADMAP item 77).
		let ticket = Source::Ticket {
			system: "gh".into(),
			object_id: "42".into(),
			section: "disc".into(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		};
		assert_eq!(
			ticket.source_id(),
			Some(util::content_hash("ticket\x0042\x00disc"))
		);
		// An empty object short-circuits to None before the hash; pin that the
		// guard is the emptiness check, not a digest of an empty object.
		assert_eq!(
			Source::Ticket {
				system: "gh".into(),
				object_id: String::new(),
				section: String::new(),
				title: String::new(),
				author: String::new(),
				url: String::new(),
			}
			.source_id(),
			None
		);
	}

	#[test]
	fn unnamed_kern_id_pins_parent_then_nonce_composition() {
		// `unnamed_kern_id` = content_hash(parent_id ++ nonce_nanos) with no
		// delimiter. Inserting one (e.g. `\x00`) passes the determinism test above
		// and orphans every kern in existence (ROADMAP item 77).
		assert_eq!(unnamed_kern_id("p", 42), util::content_hash("p42"));
		assert_eq!(unnamed_kern_id("root", 0), util::content_hash("root0"));
	}

	#[test]
	fn named_child_kern_id_pins_parent_name_nonce_composition() {
		// `named_child_kern_id` = content_hash(parent_id ++ name ++ nonce_nanos),
		// again delimiter-free. Reordering the fields (name before parent) or
		// inserting a separator passes the determinism test above and orphans
		// every named child (ROADMAP item 77).
		assert_eq!(
			named_child_kern_id("p", "code", 9),
			util::content_hash("pcode9")
		);
		assert_eq!(
			named_child_kern_id("root", "generic", 7),
			util::content_hash("rootgeneric7")
		);
	}

	#[test]
	fn observe_support_and_observe_contradict_stamp_updated_at() {
		let mut e = mk_entity("e", "x", 0.0, EntityKind::Fact);
		assert!(e.updated_at.is_none(), "fresh entity has no updated_at");
		e.observe_support(0.5);
		let support_stamp = e.updated_at;
		assert!(support_stamp.is_some(), "observe_support stamps updated_at");
		std::thread::sleep(std::time::Duration::from_millis(2));
		e.observe_contradict(0.5);
		assert!(
			e.updated_at > support_stamp,
			"observe_contradict stamps updated_at afresh"
		);
	}
}
