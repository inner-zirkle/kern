//! Near-duplicate handling at ingest: find an existing entity close enough to
//! merge into (ANN search above the dedup threshold) and fold the new arrival
//! into it — updating text, confidence evidence, and TTL — instead of minting
//! a twin.

use base::base_types::*;
use graph::accept::merge_duplicate;
use graph::graph::GraphGnn;
use std::sync::Arc;

use parking_lot::RwLock;

pub fn find_duplicate(
	graph: &Arc<RwLock<GraphGnn>>,
	vec: &[f32],
	threshold: f64,
) -> Option<String> {
	let g = graph.read();
	let hits = g.entity_idx.search(vec, 1, base::base_constants::DEDUP_EF);
	hits
		.into_iter()
		.find(|h| h.score >= threshold)
		.map(|h| h.id)
}

pub fn update_existing_entity(
	graph: &Arc<RwLock<GraphGnn>>,
	entity_id: &str,
	new_text: &str,
	new_score: f64,
	incoming_kind: EntityKind,
	incoming_valid_until: Option<std::time::SystemTime>,
	on_supersede_candidate: Option<&crate::ingest_worker::DeferContradictionFn>,
) {
	let outcome = merge_duplicate(
		&mut graph.write(),
		entity_id,
		new_text,
		new_score,
		incoming_kind,
		incoming_valid_until,
	);

	// Only a SAME-KIND near-dup may supersede (a preference must not supersede a fact).
	if let Some(o) = outcome {
		if let (Some(rid), true, Some(hook)) = (o.rephrase_id, o.same_kind, on_supersede_candidate) {
			hook(&o.kern_id, &rid);
		}
	}
}

#[cfg(test)]
#[path = "tests/ingest_dedup_test.rs"]
mod ingest_dedup_tests;
