//! The LLM-leg tick tasks: name a cohesive cluster, enrich an unnamed kern,
//! seed open questions, classify contradictions into supersedes, and resolve
//! questions against new knowledge. Each takes the graph lock briefly per
//! step and treats an empty LLM reply as "skip", never as an error.

use std::sync::Arc;

use parking_lot::RwLock;

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
use ingest::place::build_chunk_entity;
use math::reason_id;

use crate::tick_cluster::{
	centroid_thought, graviton_prompt, largest_cohesive_cluster_for_naming, vector_cluster,
};
use tick::tick_queue::{task, task_extra, Queue, TaskKind};

pub use llm::{EmbedFunc, LlmFunc};

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
			let rid = reason_id(entity_id, "", ReasonKind::Question, &question);
			let reason = Reason {
				id: rid,
				from: entity_id.to_string(),
				to: String::new(),
				to_kern_id: String::new(),
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
			spawn.id = reason_id(&spawn.from, "", spawn.kind, &spawn.to_kern_id);
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

pub fn do_resolve(q: &Queue, g: &Arc<RwLock<GraphGnn>>, kern_id: &str, rid: &str) {
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
#[path = "tests/tick_tasks_test.rs"]
mod tick_tasks_tests;
