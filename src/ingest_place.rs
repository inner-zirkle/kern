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
use util;

use parking_lot::RwLock;
use std::time::SystemTime;

fn beta_params_from_confidence(conf: f32) -> (f32, f32) {
	(1.0 + conf, 1.0 + (1.0 - conf))
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
	let (conf_alpha, conf_beta) = beta_params_from_confidence(conf);
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
		// second dedup gate drops `thought` whole, so a delta minted beforehand
		// would gossip a ValidUntil for an id no kern holds. That branch tightens
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
mod tests {
	use super::*;
	use crate::ingest::Config;

	fn session_source() -> Source {
		Source::Session {
			session_id: "s".into(),
			section: "sec".into(),
			title: String::new(),
		}
	}

	fn job(text: &str, confidence: f64) -> Job {
		Job {
			text: text.into(),
			source: session_source(),
			kind: EntityKind::Claim,
			hint: String::new(),
			confidence,
			config: Config::default(),
			review: ReviewState::default(),
			replaces: None,
			result_tx: None,
			scoping: Scoping::default(),
		}
	}

	fn empty_graph() -> Arc<RwLock<GraphGnn>> {
		Arc::new(RwLock::new(GraphGnn::new()))
	}

	fn total_entity_count(g: &Arc<RwLock<GraphGnn>>) -> usize {
		let gg = g.read();
		gg.all().iter().map(|k| k.entities.len()).sum()
	}

	#[test]
	fn beta_params_map_confidence_to_prior() {
		assert_eq!(beta_params_from_confidence(1.0), (2.0, 1.0));
		assert_eq!(beta_params_from_confidence(0.0), (1.0, 2.0));
		assert_eq!(beta_params_from_confidence(0.5), (1.5, 1.5));
	}

	#[test]
	fn chunk_source_id_is_scoped_to_the_full_source_identity() {
		let sid = session_source().source_id().unwrap();
		assert_eq!(
			chunk_source_id(&session_source(), 3),
			format!("{sid}#chunk3")
		);
		let other = Source::Session {
			session_id: "s2".into(),
			section: "sec".into(),
			title: String::new(),
		};
		assert_ne!(
			chunk_source_id(&session_source(), 0),
			chunk_source_id(&other, 0),
			"same section in different sources must not collide"
		);
		let anonymous = Source::default();
		assert_eq!(
			chunk_source_id(&anonymous, 0),
			"",
			"an identity-less source gets no external id, so it never supersedes"
		);
	}

	#[test]
	fn build_chunk_entity_carries_text_vector_and_confidence() {
		let e = build_chunk_entity(
			"hello world",
			&[0.1, 0.2, 0.3],
			EntityKind::Claim,
			&session_source(),
			"sec#chunk0",
			1.0,
			None,
			&Scoping::default(),
		);
		assert_eq!(
			e.id,
			util::content_hash("hello world"),
			"id is the content hash"
		);
		assert_eq!(e.statements, vec!["hello world".to_string()]);
		assert_eq!(e.vector[..], [0.1, 0.2, 0.3]);
		assert_eq!(e.external_id, "sec#chunk0");
		assert_eq!(e.unlinked_count, 0);
		assert!(matches!(e.kind, EntityKind::Claim));
		assert!(matches!(e.status, EntityStatus::Active));
		assert_eq!(e.chunks.len(), 1, "single statement-ref chunk part");
		assert_eq!((e.conf_alpha, e.conf_beta), (2.0, 1.0));
	}

	#[test]
	fn build_chunk_entity_clamps_out_of_range_confidence() {
		let hi = build_chunk_entity(
			"x",
			&[1.0],
			EntityKind::Claim,
			&session_source(),
			"e",
			5.0,
			None,
			&Scoping::default(),
		);
		assert_eq!((hi.conf_alpha, hi.conf_beta), (2.0, 1.0));
		let lo = build_chunk_entity(
			"y",
			&[1.0],
			EntityKind::Claim,
			&session_source(),
			"e",
			-3.0,
			None,
			&Scoping::default(),
		);
		assert_eq!((lo.conf_alpha, lo.conf_beta), (1.0, 2.0));
	}

	#[test]
	fn place_chunks_inserts_each_distinct_nonempty_chunk() {
		let g = empty_graph();
		let chunks = vec!["alpha beta".to_string(), "gamma delta".to_string()];
		let vecs = vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]];
		let placed = place_chunks(
			&g,
			None,
			None,
			&job("doc", 1.0),
			&chunks,
			&vecs,
			"doc1",
			0.95,
		);
		assert_eq!(placed, 2, "both distinct chunks placed");
		assert_eq!(
			total_entity_count(&g),
			2,
			"both accepted into the root kern"
		);
	}

	#[test]
	fn chunk_in_the_old_threshold_gap_is_not_silently_dropped() {
		let g = empty_graph();
		let chunks = vec!["alpha".to_string(), "alpha restated".to_string()];
		// cosine 0.93: inside the old 0.92 accept / 0.95 ingest gap.
		let vecs = vec![vec![1.0, 0.0, 0.0], vec![0.93, 0.367_6, 0.0]];
		let placed = place_chunks(
			&g,
			None,
			None,
			&job("doc", 1.0),
			&chunks,
			&vecs,
			"doc1",
			0.95,
		);
		assert_eq!(placed, 2);
		assert_eq!(
			total_entity_count(&g),
			2,
			"below the configured dedup threshold -> stored as a new entity, not dropped"
		);
	}

	#[test]
	fn per_kind_dedup_threshold_tightens_facts_loosens_claims() {
		// cosine 0.97 between the two chunks.
		let chunks = vec!["alpha".to_string(), "alpha nearly verbatim".to_string()];
		let vecs = vec![vec![1.0, 0.0, 0.0], vec![0.97, 0.243_1, 0.0]];

		// A Fact job whose per-kind slot is Some(0.99): the 0.97 near-dup is
		// below 0.99, so it is kept as a new entity (tighter than the global).
		let mut fact_cfg = Config::default();
		fact_cfg.dedup_threshold_by_kind[EntityKind::Fact as usize] = Some(0.99);
		let mut fact_job = job("doc", 1.0);
		fact_job.kind = EntityKind::Fact;
		fact_job.config = fact_cfg;
		let g = empty_graph();
		let placed = place_chunks(
			&g,
			None,
			None,
			&fact_job,
			&chunks,
			&vecs,
			"doc1",
			fact_job.config.dedup_threshold_for(EntityKind::Fact),
		);
		assert_eq!(placed, 2, "place_chunks counts deduped-or-new, not new");
		assert_eq!(
			total_entity_count(&g),
			2,
			"0.97 < 0.99 Fact threshold -> not deduped, two entities"
		);

		// The same Fact near-dup under the global 0.95 (no per-kind override) is
		// deduped: 0.97 >= 0.95.
		let g2 = empty_graph();
		let global_job = job("doc", 1.0);
		assert!(
			global_job
				.config
				.dedup_threshold_by_kind
				.iter()
				.all(Option::is_none),
			"default is all-None = the global threshold applies"
		);
		let _placed2 = place_chunks(
			&g2,
			None,
			None,
			&global_job,
			&chunks,
			&vecs,
			"doc1",
			global_job.config.dedup_threshold_for(EntityKind::Fact),
		);
		assert_eq!(
			total_entity_count(&g2),
			1,
			"0.97 >= 0.95 global threshold -> deduped, one entity"
		);

		// A Claim with Some(0.80) dedups a 0.81-sim the global 0.95 would also
		// catch — proves the kind-keyed path fires the other direction too.
		let mut claim_cfg = Config::default();
		claim_cfg.dedup_threshold_by_kind[EntityKind::Claim as usize] = Some(0.80);
		let mut claim_job = job("doc", 1.0);
		claim_job.kind = EntityKind::Claim;
		claim_job.config = claim_cfg;
		let claim_chunks = vec!["beta".to_string(), "beta nearly verbatim".to_string()];
		// cosine 0.81.
		let claim_vecs = vec![vec![1.0, 0.0, 0.0], vec![0.81, 0.586_4, 0.0]];
		let g3 = empty_graph();
		let _placed3 = place_chunks(
			&g3,
			None,
			None,
			&claim_job,
			&claim_chunks,
			&claim_vecs,
			"doc2",
			claim_job.config.dedup_threshold_for(EntityKind::Claim),
		);
		assert_eq!(
			total_entity_count(&g3),
			1,
			"0.81 >= 0.80 Claim threshold -> deduped, one entity"
		);
	}

	fn placed_deadlines(g: &Arc<RwLock<GraphGnn>>) -> Vec<Option<SystemTime>> {
		let gg = g.read();
		gg.all()
			.iter()
			.flat_map(|k| k.entities.values().map(|e| e.valid_until))
			.collect()
	}

	#[test]
	fn a_configured_retention_stamps_valid_until_on_every_placed_entity() {
		let deadline = SystemTime::now() + std::time::Duration::from_secs(3600);
		let g = empty_graph();
		let mut j = job("doc", 1.0);
		j.config.valid_until = Some(deadline);
		place_chunks(
			&g,
			None,
			None,
			&j,
			&["alpha beta".to_string()],
			&[vec![1.0, 0.0, 0.0]],
			"doc1",
			0.95,
		);
		assert_eq!(
			placed_deadlines(&g),
			vec![Some(deadline)],
			"the ingest-time retention reaches the entity"
		);
		let id = util::content_hash("alpha beta");
		let e = stored(&g, &id);
		assert!(
			e.valid_until_lamport > 0 && !e.valid_until_producer.is_empty(),
			"the existing LWW stamping fired for the new writer"
		);

		// The stamp alone is local. A peer only ever learns a deadline from the
		// delta, so the fresh-placement path must queue one as well — naming the
		// id that entered the graph and carrying the very lamport/producer written
		// to the entity. Moving the stamp after accept must not cost this.
		let deltas: Vec<_> = g
			.read()
			.drain_pending_deltas()
			.into_iter()
			.filter(|d| d.target == 3)
			.collect();
		assert_eq!(
			deltas.len(),
			1,
			"a placed entity gossips exactly one ValidUntil delta"
		);
		assert_eq!(deltas[0].object_id, id, "named for the placed entity");
		assert_eq!(
			(deltas[0].lamport, deltas[0].producer.as_str()),
			(e.valid_until_lamport, e.valid_until_producer.as_str()),
			"the delta carries the stamp that was written, not a second one"
		);
	}

	// Same vector for both texts, so the dedup gates fire on content-identity the
	// way a near-duplicate re-ingest does, without depending on an embedder.
	const SURVIVOR: &str = "alpha beta gamma";
	const NEAR_DUP: &str = "alpha beta gamma, restated";
	const DUP_VEC: [f32; 3] = [1.0, 0.0, 0.0];

	fn ingest_chunk(g: &Arc<RwLock<GraphGnn>>, text: &str, valid_until: Option<SystemTime>) -> usize {
		let mut j = job("doc", 1.0);
		j.config.valid_until = valid_until;
		place_chunks(
			g,
			None,
			None,
			&j,
			&[text.to_string()],
			&[DUP_VEC.to_vec()],
			"doc1",
			0.95,
		)
	}

	fn stored(g: &Arc<RwLock<GraphGnn>>, id: &str) -> Entity {
		let gg = g.read();
		let kid = gg
			.kern_of_entity(id)
			.expect("entity is indexed")
			.to_string();
		gg.loaded(&kid)
			.and_then(|k| k.entities.get(id))
			.expect("entity is stored")
			.clone()
	}

	// Drains, so a test can ignore the deltas of its setup and read only its act.
	fn valid_until_delta_ids(g: &Arc<RwLock<GraphGnn>>) -> Vec<String> {
		g.read()
			.drain_pending_deltas()
			.into_iter()
			.filter(|d| d.target == 3)
			.map(|d| d.object_id)
			.collect()
	}

	#[test]
	fn dedup_onto_an_untimed_survivor_adopts_the_incoming_deadline() {
		let deadline = SystemTime::now() + std::time::Duration::from_secs(3600);
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, None);
		let sid = util::content_hash(SURVIVOR);
		assert_eq!(
			stored(&g, &sid).valid_until,
			None,
			"survivor starts untimed"
		);

		ingest_chunk(&g, NEAR_DUP, Some(deadline));

		assert_eq!(total_entity_count(&g), 1, "the near-duplicate deduped");
		let s = stored(&g, &sid);
		assert_eq!(
			s.valid_until,
			Some(deadline),
			"min(∞, t) = t — a deduped ingest's retention reaches the survivor"
		);
		assert!(s.valid_until_lamport > 0, "stamped with a fresh lamport");
		assert!(
			!s.valid_until_producer.is_empty(),
			"stamped with a producer"
		);
	}

	#[test]
	fn dedup_keeps_the_shorter_deadline_whichever_arrives_first() {
		let hour = SystemTime::now() + std::time::Duration::from_secs(3600);
		let month = SystemTime::now() + std::time::Duration::from_secs(30 * 86_400);
		let sid = util::content_hash(SURVIVOR);

		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, Some(hour));
		let before = stored(&g, &sid);
		ingest_chunk(&g, NEAR_DUP, Some(month));
		let after = stored(&g, &sid);
		assert_eq!(
			after.valid_until,
			Some(hour),
			"a longer incoming TTL must not extend the survivor — min, not last-writer"
		);
		assert_eq!(
			after.valid_until_lamport, before.valid_until_lamport,
			"no re-stamp when the deadline does not move"
		);

		let g2 = empty_graph();
		ingest_chunk(&g2, SURVIVOR, Some(month));
		ingest_chunk(&g2, NEAR_DUP, Some(hour));
		assert_eq!(
			stored(&g2, &sid).valid_until,
			Some(hour),
			"min is commutative — arrival order cannot change the outcome"
		);
	}

	#[test]
	fn dedup_without_retention_leaves_the_survivor_deadline_alone() {
		let hour = SystemTime::now() + std::time::Duration::from_secs(3600);
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, Some(hour));
		let sid = util::content_hash(SURVIVOR);
		let before = stored(&g, &sid);

		valid_until_delta_ids(&g);
		ingest_chunk(&g, NEAR_DUP, None);

		let after = stored(&g, &sid);
		assert_eq!(
			after.valid_until,
			Some(hour),
			"min(t, ∞) = t — omitting retention is no opinion, not 'make this permanent'"
		);
		assert_eq!(after.valid_until_lamport, before.valid_until_lamport);
		assert_eq!(after.valid_until_producer, before.valid_until_producer);
		assert!(
			valid_until_delta_ids(&g).is_empty(),
			"an unchanged deadline gossips nothing"
		);
	}

	#[test]
	fn a_tightening_dedup_queues_one_delta_against_the_survivor_only() {
		let deadline = SystemTime::now() + std::time::Duration::from_secs(3600);
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, None);
		valid_until_delta_ids(&g);

		ingest_chunk(&g, NEAR_DUP, Some(deadline));

		let ids = valid_until_delta_ids(&g);
		assert_eq!(
			ids,
			vec![util::content_hash(SURVIVOR)],
			"exactly one ValidUntil delta, named for the survivor"
		);
		assert!(
			!ids.contains(&util::content_hash(NEAR_DUP)),
			"no orphan delta for the id that never entered the graph"
		);
	}

	#[test]
	fn the_second_dedup_gate_tightens_too_and_orphans_no_delta() {
		let deadline = SystemTime::now() + std::time::Duration::from_secs(3600);
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, None);
		let sid = util::content_hash(SURVIVOR);

		hide_from_gate_one(&g, &sid);
		valid_until_delta_ids(&g);

		let placed = ingest_chunk(&g, NEAR_DUP, Some(deadline));
		assert_eq!(placed, 1);
		assert_eq!(
			total_entity_count(&g),
			1,
			"gate 2 deduped — the incoming entity was dropped"
		);

		let s = stored(&g, &sid);
		assert_eq!(
			s.valid_until,
			Some(deadline),
			"the second gate carries the retention as well"
		);
		assert!(s.valid_until_lamport > 0, "stamped with a fresh lamport");
		assert!(
			!s.valid_until_producer.is_empty(),
			"stamped with a producer"
		);
		assert_eq!(
			valid_until_delta_ids(&g),
			vec![sid],
			"one delta, against the survivor — never the discarded incoming id"
		);
	}

	// Embeds every text to DUP_VEC, so place_document's gates fire on the
	// fixture's geometry instead of on a live model.
	fn fixed_vec_app() -> axum::Router {
		axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|body: axum::Json<serde_json::Value>| async move {
				let n = body
					.0
					.get("input")
					.and_then(|v| v.as_array())
					.map(|a| a.len())
					.unwrap_or(1);
				let embs: Vec<Vec<f32>> = (0..n).map(|_| DUP_VEC.to_vec()).collect();
				axum::Json(serde_json::json!({ "embeddings": embs }))
			}),
		)
	}

	// Same rig as the delta test above: hide the survivor from `find_duplicate`,
	// which reads entity_idx alone, so the incoming entity walks past gate 1 into
	// accept_with_dedup's wider scan.
	fn hide_from_gate_one(g: &Arc<RwLock<GraphGnn>>, sid: &str) {
		let mut gg = g.write();
		gg.entity_idx.delete(sid);
		gg.gnn_entity_idx
			.insert(sid.to_string(), DUP_VEC.to_vec().into());
		assert!(
			gg.entity_idx.is_empty(),
			"fixture is only honest while gate 1 has nothing left to hit"
		);
	}

	fn lexical_ids_for(g: &Arc<RwLock<GraphGnn>>, term: &str) -> Vec<String> {
		let lex = g.read().lexical().expect("in-ram lexical index");
		lex
			.search(term, 10)
			.into_iter()
			.map(|h| h.entity_id)
			.collect()
	}

	#[tokio::test]
	async fn place_document_second_gate_returns_the_survivor_and_indexes_no_orphan() {
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, None);
		let sid = util::content_hash(SURVIVOR);
		hide_from_gate_one(&g, &sid);

		let (url, _server) = crate::test_support::spawn_http(fixed_vec_app()).await;
		let embedder = LlmClient::new_embed_only(&url, "m", "");
		let doc_id = util::content_hash(NEAR_DUP);
		let (id, fail) = place_document(&g, &embedder, &job(NEAR_DUP, 1.0), &doc_id, 0.95, None).await;

		assert!(fail.is_none(), "the stub embedder answers");
		assert_eq!(total_entity_count(&g), 1, "gate 2 dropped the incoming doc");
		assert_eq!(
			id,
			Some(sid),
			"the returned id must be the one that actually entered the graph"
		);
		assert!(
			!lexical_ids_for(&g, "restated").contains(&doc_id),
			"the discarded content hash names nothing in the graph — indexing it hands retrieval a dead id"
		);
	}

	#[test]
	fn place_chunks_second_gate_keeps_the_discarded_id_out_of_the_lexical_index() {
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, None);
		let sid = util::content_hash(SURVIVOR);
		hide_from_gate_one(&g, &sid);

		assert_eq!(ingest_chunk(&g, NEAR_DUP, None), 1);
		assert_eq!(total_entity_count(&g), 1, "gate 2 deduped");
		assert!(
			!lexical_ids_for(&g, "restated").contains(&util::content_hash(NEAR_DUP)),
			"the discarded content hash names nothing in the graph — indexing it hands retrieval a dead id"
		);
	}

	#[test]
	fn no_retention_leaves_valid_until_unset() {
		let g = empty_graph();
		place_chunks(
			&g,
			None,
			None,
			&job("doc", 1.0),
			&["alpha beta".to_string()],
			&[vec![1.0, 0.0, 0.0]],
			"doc1",
			0.95,
		);
		assert_eq!(
			placed_deadlines(&g),
			vec![None],
			"a default ingest sets no valid_until"
		);
	}

	#[test]
	fn place_chunks_skips_empty_vectors() {
		let g = empty_graph();
		let chunks = vec!["a".to_string(), "b".to_string()];
		let vecs = vec![Vec::new(), vec![1.0, 0.0]];
		let placed = place_chunks(
			&g,
			None,
			None,
			&job("doc", 1.0),
			&chunks,
			&vecs,
			"doc1",
			0.95,
		);
		assert_eq!(placed, 1, "only the chunk with a real vector is placed");
		assert_eq!(total_entity_count(&g), 1);
	}

	#[test]
	fn place_chunks_defers_question_seeding_via_the_hook() {
		use std::sync::Mutex;
		let g = empty_graph();
		let chunks = vec!["the sky is blue".to_string()];
		let vecs = vec![vec![1.0, 0.0, 0.0]];
		let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
		let seen_c = seen.clone();
		let defer: crate::ingest_worker::DeferQuestionsFn =
			Arc::new(move |id: &str| seen_c.lock().unwrap().push(id.to_string()));

		let placed = place_chunks(
			&g,
			Some(&defer),
			None,
			&job("doc", 1.0),
			&chunks,
			&vecs,
			"doc1",
			0.95,
		);
		assert_eq!(placed, 1);

		let ids = seen.lock().unwrap();
		assert_eq!(ids.len(), 1, "one defer per placed chunk");
		assert!(!ids[0].is_empty(), "hook receives the placed entity id");
	}

	#[tokio::test]
	async fn place_document_reports_failure_and_leaves_graph_untouched_on_embed_error() {
		let g = empty_graph();
		let embedder = LlmClient::new_embed_only("http://127.0.0.1:1", "test", "");
		let (id, fail) =
			place_document(&g, &embedder, &job("a document", 1.0), "doc1", 0.95, None).await;
		assert!(
			id.is_none(),
			"no entity id is returned when embedding fails"
		);
		assert!(fail.is_some(), "a failure report is surfaced");
		assert_eq!(
			total_entity_count(&g),
			0,
			"graph is untouched on embed failure"
		);
	}
}
