//! Build chunk entities from split text and place them through `accept`:
//! stable per-chunk source ids, context/statement parts, and the metadata
//! (scopes, retention, review state) stamped on before placement.

use crate::ingest::Job;
use crate::ingest_dedup::{find_duplicate, update_existing_entity};
use crate::ingest_worker::embed_with_retry;
use crate::ingest_worker::FailureReport;
use base::base_types::*;
use base::crdt::GCounter;
use graph::accept;
use graph::graph::GraphGnn;
use llm::Client as LlmClient;
use std::sync::Arc;

use parking_lot::RwLock;
use std::time::SystemTime;

/// Veracity weight per source scheme (after mnemosyne's veracity tiers, MIT):
/// how much pseudo-evidence one arrival on this channel is worth. `inline`
/// (a principal's own deliberate ingest) is a full observation; a distilled
/// claim is the LLM's *inference* about a transcript, not a statement anyone
/// made; a watched file or ticket is a tool observation that may be stale.
/// The weight scales evidence STRENGTH, not the estimate: lower weight seeds
/// closer to the Jeffreys prior with wider variance, and kern's lower-bound
/// scoring penalizes exactly that.
fn veracity_weight(scheme: &str) -> f32 {
	match scheme {
		"inline" => 1.0,
		"session" => 0.7,
		"file" | "ticket" => 0.6,
		// A named agent channel: deliberate but unverified.
		"agent" => 0.8,
		_ => 0.8,
	}
}

fn beta_params_from_confidence(conf: f32, veracity: f32) -> (f32, f32) {
	(1.0 + veracity * conf, 1.0 + veracity * (1.0 - conf))
}

// The ONLY place ingest materializes an Entity — Entity is bincode-positional;
// drifting field literals silently corrupt every persisted shard.
#[allow(clippy::too_many_arguments)]
fn new_statement_entity(
	id: String,
	text: &str,
	vector: Embedding,
	kind: EntityKind,
	source: Source,
	external_id: String,
	confidence: f64,
	valid_until: Option<SystemTime>,
	unlinked_count: i32,
	scoping: &Scoping,
) -> Entity {
	let conf = confidence.clamp(0.0, 1.0) as f32;
	let (conf_alpha, conf_beta) = beta_params_from_confidence(conf, veracity_weight(source.scheme()));
	let mut t = Entity {
		id,
		root_id: String::new(),
		external_id,
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
		vector,
		gnn_vector: Embedding::default(),
		score: 0.0,
		conf_alpha,
		conf_beta,
		source,
		created_at: Some(SystemTime::now()),
		access_count: GCounter::new(),
		accessed_at: None,
		heat: 0.0,
		heat_updated_at: None,
		updated_at: None,
		valid_until,
		valid_until_lamport: 0,
		valid_until_producer: String::new(),
		producer_id: String::new(),
		unlinked_count,
		dirty: false,
		user_id: scoping.user_id.clone(),
		agent_id: scoping.agent_id.clone(),
		session_id: scoping.session_id.clone(),
		valid_from: None,
		valid_to: None,
		invalidated_at: None,
	};
	t.refresh_score();
	t
}

pub(crate) async fn place_document(
	graph: &Arc<RwLock<GraphGnn>>,
	embedder: &LlmClient,
	job: &Job,
	doc_id: &str,
	dedup_threshold: f64,
	defer_contradiction: Option<&crate::ingest_worker::DeferContradictionFn>,
) -> (Option<String>, Option<FailureReport>) {
	let vec = match embed_with_retry(embedder, &job.text, "document", 0).await {
		Ok(v) => v,
		Err(fail) => return (None, Some(fail)),
	};

	let (kind, unlinked) = document_kind(job);

	if let Some(existing_id) = find_duplicate(graph, &vec, dedup_threshold) {
		update_existing_entity(
			graph,
			&existing_id,
			&job.text,
			job.confidence,
			kind,
			job.config.valid_until,
			defer_contradiction,
		);
		return (Some(existing_id), None);
	}

	let external_id = job.source.source_id().unwrap_or_default();
	let new_external_id = external_id.clone();

	// A rename carries the old path's external_id so the stale `Document` it
	// names can be superseded. The new entity's vector is needed for the
	// Supersedes reason midpoint, so clone it only when there is something to
	// supersede — never on the common path.
	let rename_vec = if job.replaces.is_some() {
		Some(vec.clone())
	} else {
		None
	};

	let mut thought = new_statement_entity(
		doc_id.to_string(),
		&job.text,
		vec.into(),
		kind,
		job.source.clone(),
		external_id,
		job.confidence,
		job.config.valid_until,
		unlinked,
		&job.scoping,
	);
	thought.valid_from = job.config.valid_from;
	thought.review = job.review;

	let root_id = graph.read().root.id.clone();

	let tid = thought.id.clone();
	let joined = thought.statements.join(" ");

	let (result, lex, renamed_old_id) = {
		let mut g = graph.write();
		// Stamp AFTER accept, against the id that actually entered the graph: the
		// second dedup gate drops `thought` whole, so a stamp minted beforehand
		// would write a ValidUntil for an id no kern holds. That branch tightens
		// the survivor itself, inside merge_duplicate.
		let r = accept::accept_with_dedup(&mut g, &root_id, thought, "", dedup_threshold);
		if !r.deduped {
			accept::merge_valid_until(&mut g, &r.entity_id, job.config.valid_until);
		}
		// A renamed-and-edited file supersedes the entity at its old path — a
		// move-plus-edit gets a new id under a new external id and otherwise the
		// old `Document` would dangle forever (ROADMAP item 84). Only on the
		// non-dedup path: a rename whose new content paraphrases an existing
		// entity is a rarer edge left to the dedup survivor. `job.replaces` is the
		// old path's `source_id()` (external id), resolved by the sink.
		let renamed_old_id = if !r.deduped {
			job.replaces.as_deref().and_then(|old_external| {
				rename_vec.as_deref().and_then(|nv| {
					accept::supersede_renamed(
						&mut g,
						&root_id,
						&r.entity_id,
						nv,
						old_external,
						&new_external_id,
						"renamed",
					)
				})
			})
		} else {
			None
		};
		let l = g.lexical();
		(r, l, renamed_old_id)
	};
	// Only the id that entered the graph gets indexed or acked. On a gate-2 dedup
	// `tid` was discarded whole, so lexically indexing it would hand retrieval a
	// dead id, and returning it would ack a document no kern holds.
	if !result.deduped {
		if let Some(lex) = lex {
			lex.insert(&tid, &joined);
			// The superseded old path is no longer a live lexical seed.
			if let Some(old_id) = renamed_old_id {
				lex.remove(&old_id);
			}
		}
	}

	(Some(result.entity_id), None)
}

pub(crate) fn document_kind(job: &Job) -> (EntityKind, i32) {
	match job.kind {
		EntityKind::Fact => (EntityKind::Fact, -1),
		_ => (EntityKind::Document, 0),
	}
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn place_chunks(
	graph: &Arc<RwLock<GraphGnn>>,
	defer_questions: Option<&crate::ingest_worker::DeferQuestionsFn>,
	defer_contradiction: Option<&crate::ingest_worker::DeferContradictionFn>,
	job: &Job,
	chunks: &[String],
	chunk_vecs: &[Vec<f32>],
	doc_id: &str,
	dedup_threshold: f64,
) -> usize {
	let root_id = graph.read().root.id.clone();

	let mut placed = 0;
	for (i, (chunk, vec)) in chunks.iter().zip(chunk_vecs.iter()).enumerate() {
		if vec.is_empty() {
			continue;
		}

		if let Some(existing_id) = find_duplicate(graph, vec, dedup_threshold) {
			update_existing_entity(
				graph,
				&existing_id,
				chunk,
				job.confidence,
				job.kind,
				job.config.valid_until,
				defer_contradiction,
			);
			placed += 1;
			continue;
		}

		let external_id = chunk_source_id(&job.source, i);
		let mut thought = build_chunk_entity(
			chunk,
			vec,
			job.kind,
			&job.source,
			&external_id,
			job.confidence,
			job.config.valid_until,
			&job.scoping,
		);
		thought.valid_from = job.config.valid_from;
		thought.review = job.review;
		let tid = thought.id.clone();
		let joined = thought.statements.join(" ");

		let (result, lex) = {
			let mut g = graph.write();
			// Same ordering rule as place_document: the ValidUntil delta names the id
			// that actually entered the graph, never the discarded incoming one.
			let r = accept::accept_with_dedup(&mut g, &root_id, thought, doc_id, dedup_threshold);
			if !r.deduped {
				accept::merge_valid_until(&mut g, &r.entity_id, job.config.valid_until);
			}
			let l = g.lexical();
			(r, l)
		};
		// Same rule as place_document: a deduped chunk was discarded whole, so its
		// content hash names nothing — indexing it hands retrieval a dead id.
		if !result.deduped {
			if let Some(lex) = lex {
				lex.insert(&tid, &joined);
			}
			if let Some(defer) = defer_questions {
				defer(&result.entity_id);
			}
		}

		placed += 1;
	}
	placed
}

#[allow(clippy::too_many_arguments)]
pub fn build_chunk_entity(
	text: &str,
	vec: &[f32],
	kind: EntityKind,
	source: &Source,
	external_id: &str,
	confidence: f64,
	valid_until: Option<SystemTime>,
	scoping: &Scoping,
) -> Entity {
	new_statement_entity(
		util::content_hash(text),
		text,
		vec.to_vec().into(),
		kind,
		source.clone(),
		external_id.to_string(),
		confidence,
		valid_until,
		0,
		scoping,
	)
}

// Keyed on the FULL source identity (scheme+object+section), not the bare
// section: section-only ids collide across documents, so chunk 0 of every
// source superseded chunk 0 of the previous one — silent data loss.
pub fn chunk_source_id(source: &Source, index: usize) -> String {
	match source.source_id() {
		Some(sid) => format!("{sid}#chunk{index}"),
		None => String::new(),
	}
}

#[cfg(test)]
#[path = "tests/ingest_place_test.rs"]
mod ingest_place_tests;
