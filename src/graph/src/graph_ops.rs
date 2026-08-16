//! Pure graph operations: forget, link, promote, degrade — the mutations shared
//! by the CLI and MCP surfaces. They take a `GraphGnn` by `&mut` and return
//! counts; the daemon-side wiring (route/load/persist) lives in `commands`.

use crate::graph::GraphGnn;
use crate::reason::{add_reason, remove_entity, remove_reason};
use crate::search::find_entity;
use base::base_constants::{
	DEGRADE_DECAY_BASE, DEGRADE_DECAY_POW, DEGRADE_FLOOR, DEGRADE_MIN_THRESHOLD,
};
use base::base_types::{Reason, ReasonKind, ReviewState};
use math::{average_vec, reason_id};
use util::short_id;

#[derive(Default)]
pub struct SourceForget {
	pub removed_entities: usize,
	pub removed_edges: usize,
	// Local Facts the guard refused. Without this a `--source` that hits nothing
	// but Facts prints "forgot 0" and never says why `--force` was the answer.
	pub kept_facts: usize,
}

// `forget_entity`, `forget_by_source`, `link_entities`, `promote_entity`, and
// `degrade_entity_reasons` — each one is the no-I/O mutation the named command
// routes around. The daemon wrapper is responsible for routing; these are
// responsible for what the inspection-snapshot graph actually looks like.
//
// `source_id` hashes scheme+object+section, so keying on one would forget a
// single section of a document and leave the rest. Reaches exactly as far as
// `forget_entity` does — the resident kerns; an unloaded kern is out of reach.

pub fn forget_entity(g: &mut GraphGnn, id: &str, force: bool) -> Result<usize, &'static str> {
	let (thought, kern_id) = find_entity(g, id).ok_or("thought not found")?;
	if thought.is_fact() && !force {
		return Err("cannot forget a fact");
	}
	let edges_before = g.kerns.get(&kern_id).map(|k| k.reasons.len()).unwrap_or(0);
	remove_entity(g, &kern_id, id, force);
	let edges_after = g.kerns.get(&kern_id).map(|k| k.reasons.len()).unwrap_or(0);
	// saturating: remove_entity only drops edges, never adds — guard against underflow.
	Ok(edges_before.saturating_sub(edges_after))
}

/// Remove every thought whose text contains `pattern` (case-insensitive),
/// optionally narrowed to one source. In-process data hygiene — one store load,
/// the opposite of a `forget --source` per matching thought (RECALL_PLAN F2a).
/// Facts are kept unless `force`, the same guard as [`forget_entity`]. `dry_run`
/// classifies without mutating. Returns the forget tally plus up to 10 sample
/// texts for the preview.
pub fn prune_matching(
	g: &mut GraphGnn,
	pattern: &str,
	scheme: Option<&str>,
	object_id: Option<&str>,
	force: bool,
	dry_run: bool,
) -> (SourceForget, Vec<String>) {
	let pat = pattern.to_lowercase();
	let mut samples = Vec::new();
	// (id, guarded): guarded = a Fact the guard would keep without --force.
	let mut matched: Vec<(String, bool)> = Vec::new();
	for kern in g.all() {
		for t in kern.entities.values() {
			let src_ok = match (scheme, object_id) {
				(Some(s), Some(o)) => t.source.scheme() == s && t.source.object_id() == o,
				_ => true,
			};
			if !src_ok || !t.text().to_lowercase().contains(&pat) {
				continue;
			}
			if samples.len() < 10 {
				samples.push(t.text().chars().take(80).collect());
			}
			matched.push((t.id.clone(), t.is_fact() && !force));
		}
	}

	let mut out = SourceForget::default();
	for (id, guarded) in matched {
		if dry_run {
			if guarded {
				out.kept_facts += 1;
			} else {
				out.removed_entities += 1;
			}
			continue;
		}
		match forget_entity(g, &id, force) {
			Ok(edges) => {
				out.removed_entities += 1;
				out.removed_edges += edges;
			}
			// Same classification as the dry run: the guard refused it.
			Err("cannot forget a fact") => out.kept_facts += 1,
			// The id came out of the graph one statement ago; a miss means a
			// duplicate id across kerns already took it.
			Err(_) => {}
		}
	}
	(out, samples)
}

pub fn forget_by_source(
	g: &mut GraphGnn,
	scheme: &str,
	object_id: &str,
	force: bool,
) -> SourceForget {
	// Collected first: the removal mutates every kern map we would be iterating.
	let ids: Vec<String> = g
		.all()
		.into_iter()
		.flat_map(|k| k.entities.values())
		.filter(|t| t.source.scheme() == scheme && t.source.object_id() == object_id)
		.map(|t| t.id.clone())
		.collect();

	let mut out = SourceForget::default();
	for id in ids {
		match forget_entity(g, &id, force) {
			Ok(edges) => {
				out.removed_entities += 1;
				out.removed_edges += edges;
			}
			Err("cannot forget a fact") => out.kept_facts += 1,
			// The id came out of the graph one statement ago; a miss here means a
			// duplicate id across kerns already took it. Nothing left to remove.
			Err(_) => {}
		}
	}
	out
}

/// Release a held claim. Idempotent: a row already `Active` returns `false`
/// rather than erroring, so a retried promote is not a failure. A typo that
/// resolves to nothing IS one, because silently succeeding would tell a
/// curator the claim was released when it is still held.
pub fn promote_entity(g: &mut GraphGnn, id: &str) -> Result<bool, &'static str> {
	let (thought, kern_id) = find_entity(g, id).ok_or("thought not found")?;
	// Checked on the clone, before `get_mut`: that call bumps the mutation epoch
	// and invalidates the query cache, which a no-op promote has no business doing.
	if thought.review == ReviewState::Active {
		return Ok(false);
	}
	let entity = g
		.get_mut(&kern_id)
		.and_then(|k| k.entities.get_mut(id))
		.ok_or("thought not found")?;
	entity.review = ReviewState::Active;
	Ok(true)
}

/// `score` is the assertion's strength, NOT cosine(from, to): a deliberate link
/// exists precisely to connect what content similarity cannot, so scoring it by
/// endpoint similarity guarantees the edge is weakest exactly where it is the
/// only evidence. Callers pass their source's confidence (user 1.0, agent 0.95).
pub fn link_entities(
	g: &mut GraphGnn,
	from: &str,
	to: &str,
	reason_text: String,
	reason_embed: Option<Vec<f32>>,
	score: f64,
) -> Result<(String, f64), String> {
	let (from_t, from_kern_id) =
		find_entity(g, from).ok_or_else(|| format!("from thought not found: {from}"))?;
	let (to_t, _) = find_entity(g, to).ok_or_else(|| format!("to thought not found: {to}"))?;

	let vec = link_vector(reason_embed, &from_t.vector, &to_t.vector);
	let rid = reason_id(from, to, ReasonKind::Similarity, &reason_text);
	let r = Reason {
		id: rid.clone(),
		from: from.to_string(),
		to: to.to_string(),
		kind: ReasonKind::Similarity,
		text: reason_text,
		vector: vec.into(),
		score,
		..Default::default()
	};

	let kern = g.kerns.get_mut(&from_kern_id).ok_or_else(|| {
		format!(
			"link failed: kern {} no longer present",
			short_id(&from_kern_id)
		)
	})?;
	add_reason(kern, r);
	Ok((rid, score))
}

fn link_vector(reason_embed: Option<Vec<f32>>, from_vec: &[f32], to_vec: &[f32]) -> Vec<f32> {
	reason_embed.unwrap_or_else(|| average_vec(from_vec, to_vec))
}

/// Returns (decayed, removed) — how many edges had their score reduced and how
/// many dropped below `DEGRADE_MIN_THRESHOLD` and were removed.
pub fn degrade_entity_reasons(g: &mut GraphGnn, kern_id: &str, id: &str) -> (usize, usize) {
	let rids: Vec<String> = match g.kerns.get(kern_id) {
		Some(kern) => crate::reason::collect_reason_ids(kern, id),
		None => Vec::new(),
	};

	let mut decayed = 0usize;
	let mut removed = 0usize;
	for (i, rid) in rids.iter().enumerate() {
		let decay = DEGRADE_DECAY_BASE * DEGRADE_DECAY_POW.powi(i as i32);

		let should_remove = g
			.kerns
			.get(kern_id)
			.and_then(|kern| kern.reasons.get(rid))
			.map(|r| r.score - decay < DEGRADE_MIN_THRESHOLD)
			.unwrap_or(false);

		if should_remove {
			if let Some(kern) = g.kerns.get_mut(kern_id) {
				remove_reason(kern, rid);
			}
			// A degraded Rephrase takes its alternate wording out of the index with it.
			crate::lexical::reindex_entity(g, kern_id, id);
			removed += 1;
		} else {
			let lamport = g.bump_lamport();
			let producer = g.replica_id.clone();
			if let Some(kern) = g.kerns.get_mut(kern_id) {
				if let Some(r) = kern.reasons.get_mut(rid) {
					r.score = (r.score - decay).max(DEGRADE_FLOOR);
					r.score_lamport = lamport;
					r.score_producer = producer;
				}
			}
		}
		decayed += 1;
	}
	if decayed > 0 || removed > 0 {
		// Direct `kerns` mutation — same epoch contract as `remove_entity`.
		g.bump_mutation_epoch();
	}
	(decayed, removed)
}

/// A snapshot row for the graviton admin view: name, mass, and the live counts
/// of thoughts and edges it currently pulls in. Rendered as a flat array for
/// the JSON-RPC client.
pub struct GravitonRow {
	pub name: String,
	pub mass: f64,
	pub thoughts: usize,
	pub reasons: usize,
}

pub fn graviton_rows(g: &crate::graph::GraphGnn) -> Vec<GravitonRow> {
	crate::accept::root_graviton_ids(g)
		.iter()
		.filter_map(|cid| g.loaded(cid))
		.map(|c| GravitonRow {
			name: c.graviton_text.clone(),
			mass: c.mass,
			thoughts: c.entities.len(),
			reasons: c.reasons.len(),
		})
		.collect()
}
// ==== [hygiene audit] ====

/// One thought the noise audit ranked. `preview` is capped at 80 chars — the
/// report names noise, it does not reprint it (a flagged secret must not be
/// re-leaked by the audit that found it).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditCandidate {
	pub id: String,
	pub preview: String,
	pub score: f64,
	pub reasons: Vec<String>,
	pub secrets: Vec<&'static str>,
	pub action: hygiene::SuggestedAction,
	pub kind: &'static str,
	pub confidence: f64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditReport {
	pub scanned: usize,
	pub candidates: Vec<AuditCandidate>,
}

/// Score every Active resident thought for noise likelihood and return the
/// ranked candidates at or above `min_score`. Read-only and deterministic —
/// regex and arithmetic, no LLM, no embeddings. A row carrying a secret is
/// always included regardless of `min_score`: a leaked credential must surface
/// even in a lenient audit. Reaches exactly as far as `forget_entity` does —
/// the resident kerns; an unloaded kern is out of reach.
pub fn audit_noise(g: &GraphGnn, min_score: f64, limit: usize) -> AuditReport {
	let mut scanned = 0usize;
	let mut candidates: Vec<AuditCandidate> = Vec::new();
	for kern in g.all() {
		for t in kern.entities.values() {
			if t.is_superseded() {
				continue;
			}
			scanned += 1;
			let text = t.text();
			let scored = hygiene::score_noise(&text, t.conf_mean());
			if scored.score < min_score && scored.secrets.is_empty() {
				continue;
			}
			candidates.push(AuditCandidate {
				id: t.id.clone(),
				preview: text.chars().take(80).collect(),
				score: scored.score,
				action: hygiene::suggest_action(scored.score, !scored.secrets.is_empty()),
				reasons: scored.reasons,
				secrets: scored.secrets,
				kind: t.kind.as_str(),
				confidence: t.conf_mean(),
			});
		}
	}
	candidates.sort_by(|a, b| {
		b.score
			.partial_cmp(&a.score)
			.unwrap_or(std::cmp::Ordering::Equal)
	});
	candidates.truncate(limit);
	AuditReport {
		scanned,
		candidates,
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditAction {
	Archive,
	Delete,
}

impl AuditAction {
	pub fn parse(s: &str) -> Option<Self> {
		match s {
			"archive" => Some(AuditAction::Archive),
			"delete" => Some(AuditAction::Delete),
			_ => None,
		}
	}

	/// The action's own score floor. `--min-score` can only raise it: archive
	/// from 0.5, delete from 0.8 — the same ladder `suggest_action` ranks by,
	/// so an apply can never act below what the report would have suggested.
	pub fn floor(self) -> f64 {
		match self {
			AuditAction::Archive => 0.5,
			AuditAction::Delete => 0.8,
		}
	}
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct AuditApply {
	pub archived: usize,
	pub deleted: usize,
	// Local Facts the delete guard refused — deletion needs an explicit
	// per-id `forget`, never a bulk sweep.
	pub kept_facts: usize,
	// Secret-bearing rows a delete skipped: deleting a leaked credential
	// destroys the evidence needed to rotate it, so they are only reported.
	pub secrets_kept: usize,
}

/// Apply one action to every candidate at or above `max(min_score,
/// action.floor())`. Archive sets `ReviewState::Pending` — kern's reversible
/// curation hold (release with `promote`, filter with `exclude_pending`);
/// delete is `forget_entity` with the Fact guard honored and secret-bearing
/// rows always skipped.
pub fn apply_audit(g: &mut GraphGnn, min_score: f64, action: AuditAction) -> AuditApply {
	let threshold = min_score.max(action.floor());
	let report = audit_noise(g, threshold, usize::MAX);
	let mut out = AuditApply::default();
	for c in &report.candidates {
		if c.score < threshold {
			// Secret rows below the threshold ride along in every report; an
			// apply still respects the bar.
			continue;
		}
		match action {
			AuditAction::Archive => {
				if demote_entity(g, &c.id).unwrap_or(false) {
					out.archived += 1;
				}
			}
			AuditAction::Delete => {
				if !c.secrets.is_empty() {
					out.secrets_kept += 1;
					continue;
				}
				match forget_entity(g, &c.id, false) {
					Ok(_) => out.deleted += 1,
					Err("cannot forget a fact") => out.kept_facts += 1,
					Err(_) => {}
				}
			}
		}
	}
	out
}

/// The inverse of [`promote_entity`]: hold a thought as `Pending` so an
/// `exclude_pending` query drops it. Idempotent the same way — a row already
/// held returns `false` without bumping the mutation epoch.
pub fn demote_entity(g: &mut GraphGnn, id: &str) -> Result<bool, &'static str> {
	let (thought, kern_id) = find_entity(g, id).ok_or("thought not found")?;
	if thought.review == ReviewState::Pending {
		return Ok(false);
	}
	let entity = g
		.get_mut(&kern_id)
		.and_then(|k| k.entities.get_mut(id))
		.ok_or("thought not found")?;
	entity.review = ReviewState::Pending;
	Ok(true)
}

#[cfg(test)]
#[path = "tests/graph_ops_test.rs"]
mod graph_ops_tests;
