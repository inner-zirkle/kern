//! The id-direct read path: given a graph and an id, return the row as JSON.
//!
//! The same path serves both the `query` tool (so a `filter` masks a hit) and
//! the `kern get` CLI (so a routed and a local read cannot disagree about what
//! an id resolves to). A second resolver would let them disagree.

use base::base_types::Entity;
use graph::graph::GraphGnn;
use graph::reason::collect_reason_ids;
use graph::search::find_entity_by_prefix;
use serde_json::{json, Value};
use util::truncate;

const COLD_KERN: &str = "(cold)";

/// The one id resolver behind both the `query` tool and `kern get`: a second one
/// would let the routed and local reads disagree about what an id resolves to —
/// prefix or cold, resolved here or resolved by a daemon, same answer.
pub fn entity_detail_by_id(g: &GraphGnn, id: &str) -> Option<Value> {
	let hit = resolve_by_id(g, id)?;
	Some(hit.detail(g))
}

/// A resolved id read, before it is rendered. Resolving and rendering are split
/// so the `query` tool can put the row through `matches_filter` — the same
/// predicate the ranked read uses — while still resolving ids exactly one way.
pub struct IdHit {
	pub thought: Entity,
	pub kern_id: String,
	pub cold: bool,
}

impl IdHit {
	pub fn detail(&self, g: &GraphGnn) -> Value {
		let mut v = entity_detail(&self.thought, &self.kern_id, g);
		if self.cold {
			// The label is for the printer; the flag is for anything reading the
			// JSON, which should not have to match on a sentinel kern id.
			v["cold"] = Value::Bool(true);
		}
		v
	}
}

pub fn resolve_by_id(g: &GraphGnn, id: &str) -> Option<IdHit> {
	if let Some((thought, kern_id)) = find_entity_by_prefix(g, id) {
		return Some(IdHit {
			thought,
			kern_id,
			cold: false,
		});
	}
	let thought = g.store().and_then(|s| s.cold_get(id).ok().flatten())?;
	Some(IdHit {
		thought,
		kern_id: COLD_KERN.to_string(),
		cold: true,
	})
}

fn entity_detail(thought: &Entity, kern_id: &str, g: &GraphGnn) -> Value {
	let mut edges = Vec::new();
	if let Some(kern) = g.kerns.get(kern_id) {
		let rids = collect_reason_ids(kern, &thought.id);
		for rid in &rids {
			if let Some(re) = kern.reasons.get(rid) {
				edges.push(json!({
					"id": re.id,
					"from": re.from,
					"to": re.to,
					"kind": re.kind as i32,
					"text": re.text.clone(),
					"score": re.score,
				}));
			}
		}
	}
	let mut v = json!({
		"id": thought.id,
		"kind": thought.kind as u8,
		"text": thought.text(),
		"score": thought.score,
		"conf": thought.conf_mean(),
		"conf_uncertainty": thought.conf_variance(),
		"access_count": thought.access_count.value_i32(),
		"kern": kern_id,
		"source": {
			"scheme": thought.source.scheme(),
			"object_id": thought.source.object_id(),
			"section": thought.source.section(),
			"url": thought.source.url(),
		},
		"edges": edges,
	});
	// Retention on the id surface. The ranked path DROPS an expired thought
	// (`score::drop_expired`); an explicit id names one row, so answering "thought
	// not found" for a row that is demonstrably on disk — and that GC never
	// collects, since a non-superseded Fact is GC-immune — would be a lie the
	// caller cannot falsify. It is annotated instead, the way a cold hit is:
	// served, flagged, deadline included, caller decides.
	if let Some(exp) = thought.valid_until {
		v["valid_until"] = json!(secs_since_epoch(exp));
		v["expired"] = Value::Bool(exp < std::time::SystemTime::now());
	}
	v
}

fn secs_since_epoch(t: std::time::SystemTime) -> u64 {
	t.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// Renders a ranked hit row as JSON for the wire.
/// Kind/scheme/status labels are consumed by `kern_rpc::query` — do not drop them.
/// The full `source` backlink (object_id/section/url) rides alongside them, the exact
/// shape the id-lookup path emits, so a ranked hit can be followed back to the proving
/// corpus page rather than surfacing only the scheme it matched on. It is placed first
/// in the envelope on purpose: the wire contract is "ranked recall carries the backlink".
pub fn base_entity_json(entity: &Entity, score: f64) -> Value {
	let status_str = if entity.is_superseded() {
		"superseded"
	} else {
		"active"
	};
	json!({
		"id": entity.id,
		"source": {
			"scheme": entity.source.scheme(),
			"object_id": entity.source.object_id(),
			"section": entity.source.section(),
			"url": entity.source.url(),
		},
		"score": score,
		"conf": entity.conf_mean(),
		"conf_uncertainty": entity.conf_variance(),
		"text": truncate(&entity.text(), 500),
		"kind": entity.kind.as_str(),
		"scheme": entity.source.scheme(),
		"status": status_str,
	})
}
