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
use util::GNN_LADDER;

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
			GNN_LADDER.step_down("GNN propagation failed");
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
mod tests {
	use super::*;
	use base::base_types::{mk_entity, EntityKind, Reason};
	use graph::reason::add_reason;

	fn kern_with_n(n: usize) -> Kern {
		let mut k = Kern::new("k", "");
		for i in 0..n {
			let id = format!("e{i}");
			k.entities
				.insert(id.clone(), mk_entity(&id, &id, 0.0, EntityKind::Claim));
		}
		for i in 0..n.saturating_sub(1) {
			let from = format!("e{i}");
			let to = format!("e{}", i + 1);
			add_reason(
				&mut k,
				Reason {
					from: from.clone(),
					to: to.clone(),
					id: format!("{from}->{to}"),
					..Default::default()
				},
			);
		}
		k
	}

	#[test]
	fn gnn_skipped_below_min_thoughts_default() {
		let k = kern_with_n(3);
		let cfg = GnnConfig::defaults();
		assert!(
			build_gnn_snapshot(&k, &cfg).is_none(),
			"3-node graph skips GNN under the default min_thoughts floor"
		);
	}

	#[test]
	fn gnn_runs_when_floor_lowered() {
		let k = kern_with_n(3);
		let mut cfg = GnnConfig::defaults();
		cfg.min_thoughts = 2;
		assert!(
			build_gnn_snapshot(&k, &cfg).is_some(),
			"with a low floor and local edges, a snapshot builds"
		);
	}

	// The negative control for sources 3 and 4 of ROADMAP item 102 — the half no
	// seed can fix. Two identically built kerns hash their keys in different
	// orders inside one process, so with the sorts reverted `ids` and
	// `pos_edges` disagree here; a seeded rng over a shuffled snapshot is still
	// a different training run.
	#[test]
	fn two_identical_kerns_snapshot_in_the_same_order() {
		let cfg = GnnConfig {
			min_thoughts: 2,
			..GnnConfig::defaults()
		};
		let a = build_gnn_snapshot(&kern_with_n(24), &cfg).expect("snapshot builds");
		let b = build_gnn_snapshot(&kern_with_n(24), &cfg).expect("snapshot builds");
		assert_eq!(a.ids, b.ids, "node order must not be hash order");
		assert_eq!(
			a.pos_edges, b.pos_edges,
			"edge order must not be hash order"
		);
		assert_eq!(a.seed, b.seed, "one corpus seeds one training run");
		let other = build_gnn_snapshot(&kern_with_n(23), &cfg).expect("snapshot builds");
		assert_ne!(
			a.seed, other.seed,
			"a different corpus gets its own seed — kerns are not initialised alike"
		);
	}

	#[test]
	fn superseded_entities_excluded_from_gnn_snapshot() {
		let mut k = kern_with_n(4);
		k.entities.get_mut("e3").unwrap().status = EntityStatus::Superseded;
		let mut cfg = GnnConfig::defaults();
		cfg.min_thoughts = 2;
		let snap = build_gnn_snapshot(&k, &cfg).expect("active e0..e2 still build a snapshot");
		assert!(
			!snap.ids.contains(&"e3".to_string()),
			"superseded leaf excluded from GNN membership"
		);
		for id in ["e0", "e1", "e2"] {
			assert!(snap.ids.contains(&id.to_string()), "active {id} included");
		}
	}

	#[test]
	fn cosine_align_maps_similarity_into_zero_one() {
		assert_eq!(
			cosine_align(&[1.0, 0.0], &[1.0, 0.0]),
			1.0,
			"identical -> 1.0"
		);
		assert_eq!(
			cosine_align(&[1.0, 0.0], &[-1.0, 0.0]),
			0.0,
			"opposite -> 0.0"
		);
		assert!(
			(cosine_align(&[1.0, 0.0], &[0.0, 1.0]) - 0.5).abs() < 1e-6,
			"orthogonal -> 0.5"
		);
		assert_eq!(cosine_align(&[], &[]), 0.5, "empty -> 0.5");
		assert_eq!(
			cosine_align(&[1.0, 2.0], &[1.0]),
			0.5,
			"length mismatch -> 0.5"
		);
		assert_eq!(
			cosine_align(&[0.0, 0.0], &[1.0, 1.0]),
			0.5,
			"zero-norm -> 0.5"
		);
	}

	#[test]
	fn a_failed_propagation_writes_nothing_and_is_recorded_as_a_degradation() {
		let mut k = kern_with_n(3);
		// Every pair is a positive edge, so no negative edge can be sampled.
		add_reason(
			&mut k,
			Reason {
				from: "e0".into(),
				to: "e2".into(),
				id: "e0->e2".into(),
				..Default::default()
			},
		);
		k.gnn_weights = vec![7, 7, 7];
		let mut g = GraphGnn::new();
		g.kerns.insert("k".into(), k);
		let g = Arc::new(RwLock::new(g));
		let q = Queue::new(16);
		let cfg = GnnConfig {
			min_thoughts: 2,
			..GnnConfig::defaults()
		};

		do_gnn_propagate(&q, &g, "k", &cfg);

		{
			let gg = g.read();
			let kern = &gg.kerns["k"];
			assert_eq!(
				kern.gnn_weights,
				vec![7, 7, 7],
				"a failed run must not persist weights over the good ones"
			);
			assert!(
				kern.entities.values().all(|e| e.gnn_vector.is_empty()),
				"no embedding is degraded by a run that never finished"
			);
		}

		let (failed, last) = q.failures();
		assert_eq!(failed, 1, "the failure is counted, not just logged");
		assert!(
			last.expect("retained").message.contains("negative edges"),
			"the last error is kept for health reporting"
		);
		let mut rx = q.take_receiver().unwrap();
		assert!(
			rx.try_recv().is_err(),
			"no Persist is enqueued when nothing changed"
		);
	}

	#[test]
	fn apply_gnn_updates_writes_gnn_vector_weights_and_enqueues_persist() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities
			.insert("e0".into(), mk_entity("e0", "e0", 0.0, EntityKind::Claim));
		g.kerns.insert("k".into(), k);
		let g = Arc::new(RwLock::new(g));

		let new_vec = vec![0.25f64, 0.5, 0.75];
		let mut updates = HashMap::new();
		updates.insert("e0".to_string(), new_vec.clone());
		let q = Queue::new(16);

		apply_gnn_updates(&q, &g, "k", updates, vec![9, 9]);

		{
			let gg = g.read();
			let kern = gg.kerns.get("k").unwrap();
			assert_eq!(
				kern.entities["e0"].gnn_vector,
				vec![0.25f32, 0.5, 0.75].into(),
				"gnn_vector overwritten (narrowed at the boundary)"
			);
			assert_eq!(kern.gnn_weights, vec![9, 9], "kern gnn_weights stored");
		}

		let mut rx = q.take_receiver().unwrap();
		let mut persisted = false;
		while let Ok(t) = rx.try_recv() {
			if matches!(t.kind, TaskKind::Persist) {
				persisted = true;
			}
		}
		assert!(persisted, "a Persist task is enqueued after updates land");
	}

	#[test]
	fn apply_gnn_updates_skips_empty_update_vectors() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities
			.insert("e0".into(), mk_entity("e0", "e0", 0.0, EntityKind::Claim));
		g.kerns.insert("k".into(), k);
		let g = Arc::new(RwLock::new(g));

		let mut updates = HashMap::new();
		updates.insert("e0".to_string(), Vec::new());
		let q = Queue::new(16);
		apply_gnn_updates(&q, &g, "k", updates, Vec::new());

		let gg = g.read();
		assert!(
			gg.kerns["k"].entities["e0"].gnn_vector.is_empty(),
			"empty update doesn't write"
		);
	}
}
