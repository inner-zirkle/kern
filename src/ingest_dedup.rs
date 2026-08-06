use crate::accept::merge_duplicate;
use crate::graph::GraphGnn;
use crate::base_types::*;
use std::sync::Arc;

use parking_lot::RwLock;

pub fn find_duplicate(
	graph: &Arc<RwLock<GraphGnn>>,
	vec: &[f32],
	threshold: f64,
) -> Option<String> {
	let g = graph.read();
	let hits = g
		.entity_idx
		.search(vec, 1, crate::base_constants::DEDUP_EF);
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
mod tests {
	use super::*;
	use crate::base_types::mk_entity;

	fn graph_with_entity(id: &str, text: &str) -> Arc<RwLock<GraphGnn>> {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let e = mk_entity(id, text, 1.0, EntityKind::Claim);
		g.get_mut(&root).unwrap().entities.insert(id.to_string(), e);
		g.index_entity(id, &root);
		Arc::new(RwLock::new(g))
	}

	fn entity(graph: &Arc<RwLock<GraphGnn>>, id: &str) -> Entity {
		let g = graph.read();
		let kid = g.kern_of_entity(id).unwrap().to_string();
		g.kerns.get(&kid).unwrap().entities.get(id).unwrap().clone()
	}

	fn graph_with_vec_entity(id: &str, vec: Vec<f32>) -> Arc<RwLock<GraphGnn>> {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut e = mk_entity(id, "text", 1.0, EntityKind::Claim);
		e.vector = vec.into();
		g.get_mut(&root).unwrap().entities.insert(id.to_string(), e);
		g.rebuild_index();
		Arc::new(RwLock::new(g))
	}

	#[test]
	fn find_duplicate_matches_above_threshold_and_skips_below() {
		let graph = graph_with_vec_entity("e1", vec![1.0, 0.0, 0.0]);
		assert_eq!(
			find_duplicate(&graph, &[1.0, 0.0, 0.0], 0.9).as_deref(),
			Some("e1")
		);
		assert_eq!(find_duplicate(&graph, &[0.0, 1.0, 0.0], 0.9), None);
		assert_eq!(find_duplicate(&graph, &[0.9, 0.1, 0.0], 0.999), None);
		assert_eq!(
			find_duplicate(&graph, &[0.9, 0.1, 0.0], 0.9).as_deref(),
			Some("e1")
		);
	}

	#[test]
	fn find_duplicate_on_empty_graph_is_none() {
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		assert_eq!(find_duplicate(&graph, &[1.0, 0.0], 0.5), None);
	}

	#[test]
	fn same_text_reinforces_confidence_without_rephrase_edge() {
		let graph = graph_with_entity("e1", "the original claim");
		let before = entity(&graph, "e1");

		update_existing_entity(
			&graph,
			"e1",
			"the original claim",
			1.0,
			EntityKind::Claim,
			None,
			None,
		);

		let after = entity(&graph, "e1");
		assert!(
			after.conf_alpha > before.conf_alpha,
			"confidence reinforced"
		);
		assert_eq!(after.text(), "the original claim", "text untouched");
		assert!(after.updated_at.is_some(), "updated_at bumped");
		let g = graph.read();
		let kid = g.kern_of_entity("e1").unwrap();
		let any_rephrase = g
			.kerns
			.get(kid)
			.unwrap()
			.reasons
			.values()
			.any(|r| r.kind == ReasonKind::Rephrase);
		assert!(!any_rephrase, "no rephrase edge for exact-same text");
	}

	#[test]
	fn different_text_preserves_id_invariant_and_records_rephrase() {
		let graph = graph_with_entity("e1", "the original claim");
		let before = entity(&graph, "e1");

		update_existing_entity(
			&graph,
			"e1",
			"a reworded version of the claim",
			1.0,
			EntityKind::Claim,
			None,
			None,
		);

		let after = entity(&graph, "e1");
		assert_eq!(after.id, "e1", "id unchanged");
		assert_eq!(
			after.text(),
			"the original claim",
			"stored text NOT overwritten"
		);
		assert_eq!(after.vector, before.vector, "vector NOT overwritten");
		assert!(
			after.conf_alpha > before.conf_alpha,
			"confidence reinforced"
		);

		let g = graph.read();
		let kid = g.kern_of_entity("e1").unwrap();
		let rephrase: Vec<_> = g
			.kerns
			.get(kid)
			.unwrap()
			.reasons
			.values()
			.filter(|r| r.kind == ReasonKind::Rephrase)
			.collect();
		assert_eq!(rephrase.len(), 1, "exactly one rephrase edge");
		assert_eq!(rephrase[0].from, "e1");
		assert_eq!(rephrase[0].text, "a reworded version of the claim");
	}

	#[test]
	fn rephrase_edge_is_idempotent_under_repeat() {
		let graph = graph_with_entity("e1", "the original claim");
		update_existing_entity(
			&graph,
			"e1",
			"reworded claim",
			1.0,
			EntityKind::Claim,
			None,
			None,
		);
		update_existing_entity(
			&graph,
			"e1",
			"reworded claim",
			1.0,
			EntityKind::Claim,
			None,
			None,
		);

		let g = graph.read();
		let kid = g.kern_of_entity("e1").unwrap();
		let count = g
			.kerns
			.get(kid)
			.unwrap()
			.reasons
			.values()
			.filter(|r| r.kind == ReasonKind::Rephrase)
			.count();
		assert_eq!(
			count, 1,
			"duplicate rephrase observations collapse to one edge"
		);
	}
}
