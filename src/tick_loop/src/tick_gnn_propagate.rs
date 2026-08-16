//! The tick's GNN leg: snapshot a kern's entities and edges without holding
//! the lock through training, hand the snapshot to the [`crate::tick_trainer::Trainer`], and fold
//! the propagated embeddings back into the graph's GNN index when they return.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;

use base::base_types::{Embedding, EntityStatus, Kern};
use gnn::gnn::graph::Graph;
use gnn::gnn::propagate::{self, GnnConfig, GnnSnapshot};
use graph::graph::GraphGnn;

use tick::tick_queue::{task, Queue, TaskKind};

pub fn do_gnn_propagate(q: &Queue, g: &Arc<RwLock<GraphGnn>>, kern_id: &str, cfg: &GnnConfig) {
	let snap = {
		let graph = g.read();
		let kern = match graph.loaded(kern_id) {
			Some(k) => k,
			None => return,
		};
		if kern.entities.len() < cfg.min_thoughts {
			return;
		}
		build_gnn_snapshot(kern, cfg)
	};

	let snap = match snap {
		Some(s) if !s.pos_edges.is_empty() => s,
		_ => return,
	};

	// On Err nothing is applied: half-trained embeddings and the weights that
	// produced them would be persisted and re-read on every following tick.
	match propagate::run_learned_propagation(&snap, cfg) {
		Ok(res) => {
			// The only trace a propagation leaves outside the graph. Failures were
			// already loud; success was silent, which is how e2e ran for months
			// against a GNN that never executed (ROADMAP item 97). `nodes` is part
			// of the record because "it ran" and "it ran on three thoughts" are
			// different answers.
			tracing::info!(
				target: "kern.gnn",
				kern = %kern_id,
				nodes = res.updates.len(),
				"learned propagation applied"
			);
			if !res.updates.is_empty() {
				apply_gnn_updates(q, g, kern_id, res.updates, res.weights);
			}
		}
		Err(e) => {
			tracing::error!(
				target: "kern.gnn",
				kern = %kern_id,
				error = %e,
				"learned propagation failed; embeddings and weights left untouched"
			);
			q.record_task_failure(&task(TaskKind::GnnPropagate, kern_id), &e);
		}
	}
}

pub fn build_gnn_snapshot(kern: &Kern, cfg: &GnnConfig) -> Option<GnnSnapshot> {
	if kern.entities.len() < cfg.min_thoughts {
		return None;
	}

	// Sorted, not `kern.entities` order. A HashMap walk put `ids`, the
	// feature-matrix rows, `dim`'s reference entity and every `pos_edges` index
	// in per-process hash order, which no seed can undo — item 29's defect in a
	// second place (ROADMAP item 102).
	let mut entity_ids: Vec<&String> = kern.entities.keys().collect();
	entity_ids.sort();

	let mut ids = Vec::with_capacity(kern.entities.len());
	let mut dim = 0usize;
	for id in entity_ids {
		let t = &kern.entities[id];
		if !t.has_vector() {
			continue;
		}
		// Superseded entities are excluded: propagating would RE-INSERT them into
		// gnn_entity_idx via `apply_gnn_updates`, undoing the supersede removal.
		if t.status == EntityStatus::Superseded {
			continue;
		}
		if dim == 0 {
			dim = t.vector.len();
		}
		if t.vector.len() != dim || dim == 0 {
			continue;
		}
		ids.push(id.clone());
	}
	if ids.len() < cfg.min_thoughts || dim == 0 {
		return None;
	}

	let id_to_idx: HashMap<&str, usize> = ids
		.iter()
		.enumerate()
		.map(|(i, id)| (id.as_str(), i))
		.collect();
	let mut gg = Graph::new();
	for id in &ids {
		let t = &kern.entities[id];
		let feat: Vec<f64> = t.vector.iter().map(|&x| x as f64).collect();
		let _ = gg.add_node(id, feat);
	}

	let mut pair_seen = HashSet::new();
	let mut pos_edges: Vec<[usize; 2]> = Vec::new();

	// Sorted for the same reason as `ids`: this walk fixes `pos_edges`' order and
	// the order edges enter `gg`.
	let mut reason_ids: Vec<&String> = kern.reasons.keys().collect();
	reason_ids.sort();

	for rid in reason_ids {
		let r = &kern.reasons[rid];
		if !r.to_kern_id.is_empty() || r.to.is_empty() {
			continue;
		}
		let i = match id_to_idx.get(r.from.as_str()) {
			Some(&i) => i,
			None => continue,
		};
		let j = match id_to_idx.get(r.to.as_str()) {
			Some(&j) => j,
			None => continue,
		};
		if i == j {
			continue;
		}

		let _ = gg.add_edge(&r.from, &r.to);
		let _ = gg.add_edge(&r.to, &r.from);

		let (a, b) = if i < j { (i, j) } else { (j, i) };
		if pair_seen.insert((a, b)) {
			pos_edges.push([a, b]);
		}
	}
	if pos_edges.is_empty() {
		return None;
	}
	gg.add_self_loops();

	let features = gg.feature_matrix();

	let seed = gnn_seed(&ids);

	Some(GnnSnapshot {
		ids,
		features,
		graph: gg,
		pos_edges,
		weights: kern.gnn_weights.clone(),
		seed,
	})
}

/// The seed every draw in one propagation comes from — derived from the CORPUS:
/// the sorted node ids, which are content hashes, streamed through the same
/// SHA-256 `base::util::content_hash` uses.
///
/// ROADMAP item 102 posed this as a choice between a constant and the kern id,
/// and neither is right. A constant gives every kern in the fleet the same
/// initial weights, so a seed that happens to initialise some graph shape badly
/// does it to all of them at once, with no diversity left to expose it. The kern
/// id looks like the safe alternative but does not deliver what the item asks
/// for: `Kern::new_unnamed` / `new_named_child` fold `now_nanos` into the id
/// (`src/base/types.rs`), so re-ingesting the same corpus into a fresh project
/// is a new id and a new seed. MEASURED: four e2e runs under a kern-id seed gave
/// recall@1 0.9306 / 0.8889 / 0.9167 / 0.9306; three under a constant gave
/// 0.9306 three times.
///
/// The corpus is the input the item's own title names. Same facts -> same
/// content-hash ids -> same seed, in any process and in a project rebuilt from
/// scratch; different kerns hold different facts, so their models stay
/// independent. It decides only the COLD start — once `gnn_weights` is non-empty
/// the run loads those instead.
fn gnn_seed(ids: &[String]) -> u64 {
	use sha2::{Digest, Sha256};
	let mut h = Sha256::new();
	for id in ids {
		h.update(id.as_bytes());
		// Length-delimited, so ["ab","c"] and ["a","bc"] are not one corpus.
		h.update([0u8]);
	}
	let mut out = [0u8; 8];
	out.copy_from_slice(&h.finalize()[..8]);
	u64::from_be_bytes(out)
}

fn apply_gnn_updates(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	updates: HashMap<String, Vec<f64>>,
	weights: Vec<u8>,
) {
	if updates.is_empty() {
		return;
	}
	let mut graph = g.write();
	let mut changed: Vec<(String, Embedding)> = Vec::new();
	// Sorted, not `updates` order. `changed` is replayed into `gnn_entity_idx`
	// below, and HNSW links each new node to what is already there and takes its
	// entry point from the first insert (`src/base/hnsw.rs`), so a HashMap walk
	// here made the index *topology* per-process random even once the embeddings
	// were not (ROADMAP item 102).
	let mut update_ids: Vec<&String> = updates.keys().collect();
	update_ids.sort();

	if let Some(kern) = graph.kerns.get_mut(kern_id) {
		for entity_id in update_ids {
			let vec = &updates[entity_id];
			if vec.is_empty() {
				continue;
			}
			if let Some(t) = kern.entities.get_mut(entity_id) {
				// Re-checked here, not only in `build_gnn_snapshot`: training no longer
				// runs under the tick loop, so an entity can be superseded between the
				// snapshot and this write, and inserting it would undo that removal.
				if t.status == EntityStatus::Superseded {
					continue;
				}
				let vec32: Embedding = vec.iter().map(|&x| x as f32).collect();
				let w = cosine_align(&t.vector, &vec32);
				if w >= 0.5 {
					t.observe_support(w);
				} else {
					t.observe_contradict(1.0 - w);
				}
				t.gnn_vector = vec32.clone();
				changed.push((entity_id.clone(), vec32));
			}
		}
		if !weights.is_empty() {
			kern.gnn_weights = weights.clone();
		}
	}
	for (id, vec) in &changed {
		graph.gnn_entity_idx.delete(id);
		graph.gnn_entity_idx.insert(id.clone(), vec.clone());
	}
	drop(graph);

	if !changed.is_empty() || !weights.is_empty() {
		q.enqueue(task(TaskKind::Persist, kern_id));
	}
}

fn cosine_align(a: &[f32], b: &[f32]) -> f64 {
	if a.is_empty() || b.is_empty() || a.len() != b.len() {
		return 0.5;
	}
	let cos = math::cosine(a, b);
	((cos + 1.0) * 0.5).clamp(0.0, 1.0)
}

#[cfg(test)]
#[path = "tests/tick_gnn_propagate_test.rs"]
mod tick_gnn_propagate_tests;
