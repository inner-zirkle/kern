//! The LLM-leg tick tasks: name a cohesive cluster, enrich an unnamed kern,
//! seed open questions, classify contradictions into supersedes, and resolve
//! questions against new knowledge. Each takes the graph lock briefly per
//! step and treats an empty LLM reply as "skip", never as an error.

use std::sync::Arc;

use parking_lot::RwLock;

use ingest::place::build_chunk_entity;
use base::base_constants::{
	DEFAULT_SEED_K, KERN_INNER_RADIUS, KERN_OUTER_RADIUS, PROVENANCE_SCORE,
	QUESTION_RESOLVE_THRESHOLD,
};
use base::base_types::{Embedding, Reason, ReasonKind, Scoping};
use config::HeatConfig;
use config::TickConfig;
use graph::accept::{
	classify_prompt, parse_contradiction, supersede_by_contradiction, ContradictionClass,
};
use graph::graph::GraphGnn;
use graph::reason::{add_reason, remove_reason};
use graph::search::search_all_unlocked;
use math::reason_id;

use crate::tick_cluster::{
	centroid_thought, graviton_prompt, largest_cohesive_cluster_for_naming, vector_cluster,
};
use tick::tick_queue::{task, task_extra, Queue, TaskKind};

pub use llm::{EmbedFunc, LlmFunc};
pub type BroadcastQuestionFunc = Arc<dyn Fn(&str, &[f32], &str) + Send + Sync>;

fn strip_name_prefixes(raw: &str) -> String {
	let mut name = raw.trim().to_string();
	for pfx in &["Theme:", "Name:", "Label:", "theme:", "name:"] {
		if let Some(after) = name.strip_prefix(pfx) {
			name = after.trim().to_string();
			break;
		}
	}
	name
}

// Lock order: snapshot under a read guard, LLM unlocked, one write guard.
pub fn do_seed_questions(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	entity_id: &str,
	llm: Option<&LlmFunc>,
) {
	let Some(llm) = llm else { return };
	let (text, root_id) = {
		let g = g.read();
		let Some(kid) = g.kern_of_entity(entity_id).map(|s| s.to_string()) else {
			return;
		};
		let Some(text) = g
			.kerns
			.get(&kid)
			.and_then(|k| k.entities.get(entity_id))
			.map(|e| e.text())
		else {
			return;
		};
		(text, g.root.id.clone())
	};
	if text.trim().is_empty() {
		return;
	}

	let prompt = format!(
		"Given this knowledge chunk, generate up to 3 questions that this chunk answers. \
		 One question per line. No numbering.\n\n{text}"
	);
	let response = llm(&prompt);
	if response.is_empty() {
		return;
	}
	let questions: Vec<String> = response
		.lines()
		.map(|l| l.trim().to_string())
		.filter(|l| !l.is_empty())
		.take(3)
		.collect();
	if questions.is_empty() {
		return;
	}

	{
		let mut g = g.write();
		for question in questions {
			let rid = reason_id(entity_id, "", ReasonKind::Question, &question, "");
			let reason = Reason {
				id: rid,
				from: entity_id.to_string(),
				to: String::new(),
				to_kern_id: String::new(),
				to_net_id: String::new(),
				kind: ReasonKind::Question,
				dirty: false,
				text: question,
				vector: Vec::new().into(),
				score: 0.5,
				score_lamport: 0,
				score_producer: String::new(),
				traversal_count: base::crdt::GCounter::new(),
				producer_id: String::new(),
			};
			if let Some(kern) = g.kerns.get_mut(&root_id) {
				add_reason(kern, reason);
			}
		}
	}
	q.enqueue(task(TaskKind::Persist, &root_id));
}

// LLM runs unlocked; fail open at every step (edge left as recorded).
pub fn do_classify_contradiction(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	rid: &str,
	llm: Option<&LlmFunc>,
	embed: Option<&EmbedFunc>,
) {
	let (llm, embed) = match (llm, embed) {
		(Some(l), Some(e)) => (l, e),
		_ => return,
	};

	let (old_id, old_text, new_text, old_kind, old_source, confidence) = {
		let graph = g.read();
		let kern = match graph.loaded(kern_id) {
			Some(k) => k,
			None => return,
		};
		let r = match kern.reasons.get(rid) {
			Some(r) => r,
			None => return,
		};
		if r.kind != ReasonKind::Rephrase || !r.to.is_empty() {
			return;
		}
		let old = match kern.entities.get(&r.from) {
			Some(e) if !e.is_superseded() => e,
			_ => return,
		};
		(
			r.from.clone(),
			old.text(),
			r.text.clone(),
			old.kind,
			old.source.clone(),
			old.conf_mean(),
		)
	};
	if new_text.trim().is_empty() || new_text == old_text {
		return;
	}

	if parse_contradiction(&llm(&classify_prompt(&old_text, &new_text)))
		!= ContradictionClass::Supersede
	{
		return;
	}

	let vec = match embed(&new_text) {
		Ok(v) if !v.is_empty() => v,
		_ => return,
	};
	let new_id = util::content_hash(&new_text);
	if new_id == old_id {
		return;
	}
	let new_thought = build_chunk_entity(
		&new_text,
		&vec,
		old_kind,
		&old_source,
		"",
		confidence,
		None,
		&Scoping::default(),
	);

	// Re-validate under the write guard — another tick may have superseded or
	// removed this pair while we were unlocked.
	{
		let mut graph = g.write();
		let still_pending = graph
			.loaded(kern_id)
			.map(|k| {
				k.reasons
					.get(rid)
					.is_some_and(|r| r.kind == ReasonKind::Rephrase)
					&& k.entities.get(&old_id).is_some_and(|e| !e.is_superseded())
			})
			.unwrap_or(false);
		if !still_pending {
			return;
		}
		let rids = supersede_by_contradiction(&mut graph, kern_id, &old_id, new_thought, &new_text);
		if !rids.is_empty() {
			if let Some(k) = graph.get_mut(kern_id) {
				remove_reason(k, rid);
			}
			// The wording just became the revision's own text — drop it from the
			// superseded entity's document, which no longer carries that Rephrase.
			graph::lexical::reindex_entity(&graph, kern_id, &old_id);
			if let Some(lex) = graph.lexical() {
				lex.insert(&new_id, &new_text);
			}
		}
	}

	q.enqueue(task(TaskKind::Persist, kern_id));
	q.enqueue(task(TaskKind::GnnPropagate, kern_id));
}

fn naming_prompt(
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	cfg: &TickConfig,
) -> Option<(String, Option<String>, String)> {
	let graph = g.read();
	let kern = graph.loaded(kern_id)?;
	if kern.is_named() {
		return None;
	}
	let entities: Vec<_> = kern.entities.values().collect();
	let clusters = vector_cluster(&entities, cfg.max_cluster_sample);
	let idx = largest_cohesive_cluster_for_naming(&clusters)?;
	let prompt = graviton_prompt(&clusters[idx]);
	let centroid_id = centroid_thought(&clusters[idx]).map(|t| t.id.clone());
	let parent_id = kern.parent.clone();
	Some((prompt, centroid_id, parent_id))
}

pub fn do_name(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	cfg: &TickConfig,
	llm: Option<&LlmFunc>,
	embed: Option<&EmbedFunc>,
) {
	let llm = match llm {
		Some(f) => f,
		None => return,
	};

	let (prompt, centroid_id, parent_id) = match naming_prompt(g, kern_id, cfg) {
		Some(t) => t,
		None => return,
	};

	let raw = llm(&prompt);
	let name_text = strip_name_prefixes(&raw);
	if name_text.is_empty() {
		return;
	}
	let name_vec = embed.and_then(|e| e(&name_text).ok());

	let promoted_to_root = {
		let mut graph = g.write();
		let kern = match graph.kerns.get_mut(kern_id) {
			Some(k) => k,
			None => return,
		};
		if kern.is_named() {
			return;
		}
		kern.graviton_text = name_text.clone();
		kern.graviton_vec = name_vec.unwrap_or_default();
		kern.inner_radius = KERN_INNER_RADIUS;
		kern.outer_radius = KERN_OUTER_RADIUS;

		if let Some(ref cid) = centroid_id {
			let mut spawn = Reason {
				kind: ReasonKind::Spawn,
				from: cid.clone(),
				to_kern_id: kern_id.to_string(),
				score: PROVENANCE_SCORE,
				..Default::default()
			};
			spawn.id = reason_id(&spawn.from, "", spawn.kind, &spawn.to_kern_id, "");
			kern.spawn_reason_id = spawn.id.clone();
			if let Some(parent) = graph.kerns.get_mut(&parent_id) {
				add_reason(parent, spawn);
			}
		}

		graph::accept::promote_to_root_if_generic(&mut graph, kern_id)
	};

	{
		let graph = g.read();
		if let Some(kern) = graph.loaded(kern_id) {
			for r in kern.reasons.values() {
				if r.is_enriched() || r.kind == ReasonKind::Spawn || r.kind == ReasonKind::Question {
					continue;
				}
				q.enqueue(task_extra(TaskKind::Enrich, kern_id, &r.id));
			}
		}
	}
	q.enqueue(task(TaskKind::Persist, kern_id));
	if !parent_id.is_empty() {
		q.enqueue(task(TaskKind::Persist, &parent_id));
	}
	// Promotion rewired the root's children — persist it too.
	if promoted_to_root {
		let root_id = g.read().root.id.clone();
		q.enqueue(task(TaskKind::Persist, &root_id));
	}
}

pub fn do_enrich(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	rid: &str,
	llm: Option<&LlmFunc>,
	embed: Option<&EmbedFunc>,
) {
	let (llm, embed) = match (llm, embed) {
		(Some(l), Some(e)) => (l, e),
		_ => return,
	};

	let prompt = {
		let graph = g.read();
		let kern = match graph.loaded(kern_id) {
			Some(k) => k,
			None => return,
		};
		let r = match kern.reasons.get(rid) {
			Some(r) => r,
			None => return,
		};
		if r.is_enriched() || r.kind == ReasonKind::Spawn || r.kind == ReasonKind::Question {
			return;
		}
		let from = match kern.entities.get(&r.from) {
			Some(t) => t,
			None => return,
		};
		let to = match kern.entities.get(&r.to) {
			Some(t) => t,
			None => return,
		};
		util::explain_relationship_prompt(&from.text(), &to.text())
	};

	let text = llm(&prompt);
	if text.is_empty() {
		return;
	}
	let text = text.trim().to_string();
	let vec = embed(&text).ok();

	{
		let mut graph = g.write();
		let mut new_vec: Option<(String, Embedding)> = None;
		if let Some(kern) = graph.kerns.get_mut(kern_id) {
			if let Some(r) = kern.reasons.get_mut(rid) {
				if !r.is_enriched() {
					r.text = text;
					if let Some(v) = vec.map(Embedding::from) {
						r.vector = v.clone();
						new_vec = Some((rid.to_string(), v));
					}
				}
			}
		}
		if let Some((rid, v)) = new_vec {
			graph.reason_idx.delete(&rid);
			graph.reason_idx.insert(rid, v);
		}
	}

	q.enqueue(task(TaskKind::Persist, kern_id));
	q.enqueue(task(TaskKind::GnnPropagate, kern_id));
}

pub fn do_resolve(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	rid: &str,
	bq: Option<&BroadcastQuestionFunc>,
) {
	let top_hit = {
		let graph = g.read();
		let kern = match graph.loaded(kern_id) {
			Some(k) => k,
			None => return,
		};
		let r = match kern.reasons.get(rid) {
			Some(r) => r,
			None => return,
		};
		if r.kind != ReasonKind::Question || !r.to.is_empty() {
			return;
		}
		let vec = r.vector.clone();
		search_all_unlocked(&graph, &vec, DEFAULT_SEED_K)
			.into_iter()
			.next()
			.filter(|h| h.score >= QUESTION_RESOLVE_THRESHOLD)
			.map(|h| h.entity_id)
	};

	// Re-validate under the write guard — another tick could have resolved or
	// removed this question while the read guard was dropped.
	if let Some(entity_id) = top_hit {
		{
			let mut graph = g.write();
			let kern = match graph.kerns.get_mut(kern_id) {
				Some(k) => k,
				None => return,
			};
			let r = match kern.reasons.get_mut(rid) {
				Some(r) => r,
				None => return,
			};
			if r.kind != ReasonKind::Question || !r.to.is_empty() {
				return;
			}
			r.to = entity_id;
			r.kind = ReasonKind::Similarity;
		}
		q.enqueue(task(TaskKind::Persist, kern_id));
		return;
	}

	let broadcast_data = if bq.is_some() {
		let graph = g.read();
		graph.loaded(kern_id).and_then(|kern| {
			kern
				.reasons
				.get(rid)
				.map(|r| (r.id.clone(), r.vector.clone(), r.text.clone()))
		})
	} else {
		None
	};

	if let (Some(bq), Some((id, rvec, rtext))) = (bq, broadcast_data) {
		bq(&id, &rvec, &rtext);
	}
}

pub fn do_disk_consolidate(g: &Arc<RwLock<GraphGnn>>) {
	g.write().consolidate_disk_index();
}

pub fn do_commit_access(g: &Arc<RwLock<GraphGnn>>, extra: &str, heat_cfg: &HeatConfig) {
	let ids: Vec<String> = extra
		.lines()
		.filter(|l| !l.is_empty())
		.map(str::to_string)
		.collect();
	if ids.is_empty() {
		return;
	}
	retrieval::score::commit_access_ids(&mut g.write(), &ids, heat_cfg);
}

pub fn do_persist(g: &Arc<RwLock<GraphGnn>>, kern_id: &str) {
	let graph = g.read();
	let store = match graph.store() {
		Some(s) => s,
		None => return,
	};
	// Stale-write guard: if another writer advanced the store, a per-kern overwrite
	// would drop newer rows — skip; reconcile_if_stale reloads and re-persists.
	if store.read_epoch() > graph.flushed_epoch() {
		tracing::debug!(
			target: "kern.persist",
			kern = %kern_id,
			disk_epoch = store.read_epoch(),
			flushed_epoch = graph.flushed_epoch(),
			"skipping per-kern persist of a stale graph (store advanced under us)"
		);
		return;
	}
	// Root authoritative fields live on `graph.root`, not the map entry — persist
	// through the same merge `save_all` uses so they can't be dropped.
	if kern_id == graph.root.id {
		let _ = store.save_one_kern(&graph::persist::merged_root(&graph));
		return;
	}
	let kern = match graph.loaded(kern_id) {
		Some(k) => k,
		None => return,
	};
	let _ = store.save_one_kern(kern);
}

pub fn do_reembed(g: &Arc<RwLock<GraphGnn>>, kern_id: &str, embed: Option<&EmbedFunc>) {
	let Some(embed) = embed else { return };

	let dirty_ents: Vec<(String, String)> = {
		let g = g.read();
		let Some(k) = g.kerns.get(kern_id) else {
			return;
		};
		k.entities
			.values()
			.filter(|e| e.dirty)
			.map(|e| (e.id.clone(), e.text()))
			.collect()
	};

	// Embed outside the lock — network I/O.
	let mut new_vecs: Vec<(String, Vec<f32>)> = Vec::new();
	for (id, text) in &dirty_ents {
		if let Ok(v) = embed(text) {
			if !v.is_empty() {
				new_vecs.push((id.clone(), v));
			}
		}
	}

	let has_dirty_reasons = {
		let g = g.read();
		g.kerns
			.get(kern_id)
			.map(|k| k.reasons.values().any(|r| r.dirty))
			.unwrap_or(false)
	};

	if new_vecs.is_empty() && !has_dirty_reasons {
		return;
	}

	{
		let mut g = g.write();
		let Some(k) = g.kerns.get_mut(kern_id) else {
			return;
		};
		for (id, v) in &new_vecs {
			if let Some(e) = k.entities.get_mut(id) {
				e.vector = v.clone().into();
				e.gnn_vector = e.vector.clone();
				e.dirty = false;
			}
		}
		let endpoint = |k: &base::base_types::Kern, id: &str| -> Option<Embedding> {
			k.entities
				.get(id)
				.map(|e| e.vector.clone())
				.filter(|v| !v.is_empty())
		};
		let reason_ids: Vec<String> = k
			.reasons
			.values()
			.filter(|r| r.dirty)
			.map(|r| r.id.clone())
			.collect();
		for rid in reason_ids {
			let (from, to) = match k.reasons.get(&rid) {
				Some(r) => (r.from.clone(), r.to.clone()),
				None => continue,
			};
			let nv = match (endpoint(k, &from), endpoint(k, &to)) {
				(Some(fv), Some(tv)) => Some(math::average_vec(&fv, &tv)),
				_ => None,
			};
			if let Some(r) = k.reasons.get_mut(&rid) {
				// endpoint not yet embedded: leave the edge dirty to retry, don't pin a stale vector.
				if let Some(v) = nv {
					r.vector = v.into();
					r.dirty = false;
				}
			}
		}
		g.rebuild_index();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use base::base_types::{Entity, Kern};
	use graph::graph::GraphGnn;
	use parking_lot::RwLock;
	use std::sync::Arc;

	#[test]
	fn do_seed_questions_adds_question_edges_for_the_entity() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut e = Entity {
			id: "e1".into(),
			..Default::default()
		};
		e.set_text("the spawn gate shipped today".into());
		g.kerns
			.get_mut(&root)
			.unwrap()
			.entities
			.insert("e1".into(), e);
		g.rebuild_index();
		let g = Arc::new(RwLock::new(g));

		let llm: LlmFunc =
			Arc::new(|_p: &str| "What shipped today?\nWhen did the gate ship?".to_string());
		let q = Queue::new(16);
		do_seed_questions(&q, &g, "e1", Some(&llm));

		let gg = g.read();
		let qs: Vec<_> = gg
			.kerns
			.get(&root)
			.unwrap()
			.reasons
			.values()
			.filter(|r| r.kind == ReasonKind::Question && r.from == "e1" && r.to.is_empty())
			.collect();
		assert_eq!(qs.len(), 2, "one dangling Question edge per LLM line");
		drop(gg);

		let mut rx = q.take_receiver().unwrap();
		let mut persists = Vec::new();
		while let Ok(t) = rx.try_recv() {
			if matches!(t.kind, TaskKind::Persist) {
				persists.push(t.kern_id.clone());
			}
		}
		assert_eq!(
			persists,
			vec![root.clone()],
			"seeded Question edges are followed by a root Persist — without it they lived only in RAM until an unrelated flush"
		);
	}

	fn graph_with_rephrase(
		old_text: &str,
		new_text: &str,
	) -> (Arc<RwLock<GraphGnn>>, String, String) {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut old = Entity {
			id: "old".into(),
			kind: base::base_types::EntityKind::Claim,
			vector: vec![1.0, 0.0].into(),
			..Default::default()
		};
		old.set_text(old_text.into());
		old.dirty = false;
		g.get_mut(&root).unwrap().entities.insert("old".into(), old);
		g.index_entity("old", &root);
		g.entity_idx.insert("old".into(), vec![1.0, 0.0].into());
		let rid = reason_id("old", "", ReasonKind::Rephrase, new_text, "");
		add_reason(
			g.get_mut(&root).unwrap(),
			Reason {
				id: rid.clone(),
				from: "old".into(),
				to: String::new(),
				kind: ReasonKind::Rephrase,
				text: new_text.into(),
				..Default::default()
			},
		);
		(Arc::new(RwLock::new(g)), root, rid)
	}

	#[test]
	fn classify_contradiction_supersedes_on_update_verdict() {
		let (g, root, rid) = graph_with_rephrase("the deadline is March", "the deadline is April");
		let llm: LlmFunc = Arc::new(|_p: &str| "CONTRADICTION".to_string());
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.9, 0.1]));
		let q = Queue::new(16);
		do_classify_contradiction(&q, &g, &root, &rid, Some(&llm), Some(&embed));

		let gg = g.read();
		let kern = gg.kerns.get(&root).unwrap();
		let old = kern.entities.get("old").unwrap();
		assert!(
			old.is_superseded(),
			"old is superseded on a CONTRADICTION verdict"
		);
		assert!(old.invalidated_at.is_some(), "old stamped invalidated");
		let new_id = util::content_hash("the deadline is April");
		assert!(
			kern.entities.contains_key(&new_id),
			"new revision materialized"
		);
		assert_eq!(old.superseded_by, new_id);
		assert!(
			!kern.reasons.contains_key(&rid),
			"the Rephrase edge is retired once it becomes a Supersedes edge"
		);
		assert!(
			kern
				.reasons
				.values()
				.any(|r| r.kind == ReasonKind::Supersedes),
			"a Supersedes edge now links the revisions"
		);
	}

	#[test]
	fn classify_contradiction_keeps_rephrase_on_related_verdict() {
		let (g, root, rid) = graph_with_rephrase("cats are mammals", "cats are feline mammals");
		let llm: LlmFunc = Arc::new(|_p: &str| "RELATED".to_string());
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.9, 0.1]));
		let q = Queue::new(16);
		do_classify_contradiction(&q, &g, &root, &rid, Some(&llm), Some(&embed));

		let gg = g.read();
		let kern = gg.kerns.get(&root).unwrap();
		assert!(
			!kern.entities.get("old").unwrap().is_superseded(),
			"a RELATED verdict leaves the stored claim active"
		);
		assert!(
			kern.reasons.contains_key(&rid),
			"the Rephrase edge stands unchanged on RELATED"
		);
	}

	#[test]
	fn classify_contradiction_is_a_noop_without_llm() {
		let (g, root, rid) = graph_with_rephrase("a", "b");
		let q = Queue::new(16);
		do_classify_contradiction(&q, &g, &root, &rid, None, None);
		let gg = g.read();
		let kern = gg.kerns.get(&root).unwrap();
		assert!(!kern.entities.get("old").unwrap().is_superseded());
		assert!(kern.reasons.contains_key(&rid), "rephrase edge preserved");
	}

	#[test]
	fn do_seed_questions_is_a_noop_without_llm_or_entity() {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let q = Queue::new(16);
		do_seed_questions(&q, &g, "e1", None);
		let llm: LlmFunc = Arc::new(|_p: &str| "Q?".to_string());
		do_seed_questions(&q, &g, "missing", Some(&llm));
		let gg = g.read();
		let root = gg.root.id.clone();
		assert!(
			gg.kerns.get(&root).unwrap().reasons.is_empty(),
			"no edges minted"
		);
	}

	#[test]
	fn do_commit_access_stamps_the_live_entities_from_the_id_list() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		g.index_entity("a", &kid);
		let epoch_before = g.mutation_epoch();
		let g = Arc::new(RwLock::new(g));

		do_commit_access(&g, "a", &HeatConfig::default());

		let gg = g.read();
		let live = gg.kerns.get(&kid).unwrap().entities.get("a").unwrap();
		assert!(
			live.accessed_at.is_some(),
			"the deferred stamp reached the live entity"
		);
		assert_eq!(
			live.access_count.value(),
			1,
			"live access counter bumped by the tick"
		);
		assert_eq!(
			gg.mutation_epoch(),
			epoch_before,
			"the access stamp must not invalidate the query cache"
		);
	}

	#[test]
	fn do_reembed_clears_dirty_and_sets_vector() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		let mut e = Entity {
			id: "e1".into(),
			dirty: true,
			..Default::default()
		};
		e.set_text("hello world".into());
		kern.entities.insert(e.id.clone(), e);
		g.kerns.insert(kid.clone(), kern);
		let g = Arc::new(RwLock::new(g));
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.1, 0.2, 0.3]));
		do_reembed(&g, &kid, Some(&embed));
		let g = g.read();
		let e = g.kerns.get(&kid).unwrap().entities.get("e1").unwrap();
		assert!(!e.dirty, "dirty must be cleared after reembed");
		assert_eq!(e.vector[..], [0.1, 0.2, 0.3]);
	}

	#[test]
	fn do_reembed_shares_vector_allocation_with_gnn_vector() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		let mut e = Entity {
			id: "e1".into(),
			dirty: true,
			..Default::default()
		};
		e.set_text("hello world".into());
		kern.entities.insert(e.id.clone(), e);
		g.kerns.insert(kid.clone(), kern);
		let g = Arc::new(RwLock::new(g));
		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![0.1, 0.2, 0.3]));
		do_reembed(&g, &kid, Some(&embed));
		let g = g.read();
		let e = g.kerns.get(&kid).unwrap().entities.get("e1").unwrap();
		assert!(
			std::sync::Arc::ptr_eq(&e.vector, &e.gnn_vector),
			"reembed must share the Arc between vector and gnn_vector, not allocate twice"
		);
	}

	#[test]
	fn do_reembed_recomputes_dirty_reason_as_endpoint_mean() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			},
		);
		kern.entities.insert(
			"b".into(),
			Entity {
				id: "b".into(),
				vector: vec![0.0, 1.0].into(),
				..Default::default()
			},
		);
		add_reason(
			&mut kern,
			Reason {
				id: "a->b".into(),
				from: "a".into(),
				to: "b".into(),
				dirty: true,
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		let g = Arc::new(RwLock::new(g));

		let embed: EmbedFunc = Arc::new(|_t: &str| Ok(vec![9.0, 9.0]));
		do_reembed(&g, &kid, Some(&embed));

		let g = g.read();
		let r = g.kerns.get(&kid).unwrap().reasons.get("a->b").unwrap();
		assert!(!r.dirty, "dirty reason cleared once recomputed");
		assert_eq!(
			r.vector,
			vec![0.5, 0.5].into(),
			"reason vector is the mean of endpoint vectors"
		);
	}

	#[test]
	fn do_resolve_links_question_to_nearest_entity_above_threshold() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"target".into(),
			Entity {
				id: "target".into(),
				vector: vec![1.0, 0.0, 0.0].into(),
				..Default::default()
			},
		);
		kern.entities.insert(
			"asker".into(),
			Entity {
				id: "asker".into(),
				vector: vec![0.0, 1.0, 0.0].into(),
				..Default::default()
			},
		);
		add_reason(
			&mut kern,
			Reason {
				id: "q1".into(),
				from: "asker".into(),
				to: String::new(),
				kind: ReasonKind::Question,
				vector: vec![1.0, 0.0, 0.0].into(),
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		g.rebuild_index();
		let g = Arc::new(RwLock::new(g));

		let q = Queue::new(16);
		do_resolve(&q, &g, &kid, "q1", None);

		let g = g.read();
		let r = g.kerns.get(&kid).unwrap().reasons.get("q1").unwrap();
		assert_eq!(
			r.kind,
			ReasonKind::Similarity,
			"resolved question becomes a Similarity edge"
		);
		assert_eq!(r.to, "target", "linked to the nearest indexed entity");
	}

	#[test]
	fn do_resolve_ignores_non_question_or_already_linked() {
		let mut g = GraphGnn::new();
		let kid = "k1".to_string();
		let mut kern = Kern::new(kid.clone(), "");
		kern.entities.insert(
			"target".into(),
			Entity {
				id: "target".into(),
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			},
		);
		add_reason(
			&mut kern,
			Reason {
				id: "linked".into(),
				from: "x".into(),
				to: "y".into(),
				kind: ReasonKind::Question,
				vector: vec![1.0, 0.0].into(),
				..Default::default()
			},
		);
		g.kerns.insert(kid.clone(), kern);
		g.rebuild_index();
		let g = Arc::new(RwLock::new(g));

		let q = Queue::new(16);
		do_resolve(&q, &g, &kid, "linked", None);

		let g = g.read();
		let r = g.kerns.get(&kid).unwrap().reasons.get("linked").unwrap();
		assert_eq!(
			r.kind,
			ReasonKind::Question,
			"already-linked question is untouched"
		);
		assert_eq!(r.to, "y", "existing link preserved");
	}

	#[test]
	fn strip_name_prefixes_removes_first_known_label_only() {
		assert_eq!(
			strip_name_prefixes("Theme: rust ownership"),
			"rust ownership"
		);
		assert_eq!(
			strip_name_prefixes("  name:  caching layer  "),
			"caching layer"
		);
		assert_eq!(strip_name_prefixes("Label:x"), "x");
		assert_eq!(strip_name_prefixes("  plain phrase "), "plain phrase");
		assert_eq!(strip_name_prefixes("Theme: Name: nested"), "Name: nested");
	}
}
