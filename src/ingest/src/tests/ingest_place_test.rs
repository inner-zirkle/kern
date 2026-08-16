//! Tests extracted from ingest_place.rs
#![allow(unused)]
use super::*;

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
		// Full veracity: one whole pseudo-observation.
		assert_eq!(beta_params_from_confidence(1.0, 1.0), (2.0, 1.0));
		assert_eq!(beta_params_from_confidence(0.0, 1.0), (1.0, 2.0));
		assert_eq!(beta_params_from_confidence(0.5, 1.0), (1.5, 1.5));
		// Lower veracity scales evidence STRENGTH toward the Jeffreys prior,
		// not the estimate: same 1.0 confidence, weaker claim on it.
		assert_eq!(beta_params_from_confidence(1.0, 0.5), (1.5, 1.0));
	}

	#[test]
	fn veracity_weights_by_channel_keep_inline_a_full_observation() {
		assert_eq!(
			veracity_weight("inline"),
			1.0,
			"a deliberate ingest is a full observation"
		);
		assert_eq!(
			veracity_weight("session"),
			0.7,
			"a distilled claim is an inference"
		);
		assert_eq!(
			veracity_weight("file"),
			0.6,
			"a watched file is a tool observation"
		);
		assert_eq!(veracity_weight("ticket"), 0.6);
		assert_eq!(veracity_weight("agent"), 0.8);
		assert_eq!(veracity_weight("anything-else"), 0.8);
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
		// Session channel: veracity 0.7 of one pseudo-observation at conf 1.0.
		assert_eq!((e.conf_alpha, e.conf_beta), (1.7, 1.0));
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
		assert_eq!((hi.conf_alpha, hi.conf_beta), (1.7, 1.0));
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
		assert_eq!((lo.conf_alpha, lo.conf_beta), (1.0, 1.7));
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

	// Every id the graph holds a ValidUntil stamp for. The stamp is the whole
	// observable now: it names which id the tightening actually landed on.
	fn valid_until_stamped_ids(g: &Arc<RwLock<GraphGnn>>) -> Vec<String> {
		let gg = g.read();
		let mut ids: Vec<String> = gg
			.all()
			.into_iter()
			.flat_map(|k| k.entities.values())
			.filter(|e| e.valid_until_lamport > 0)
			.map(|e| e.id.clone())
			.collect();
		ids.sort();
		ids
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

		ingest_chunk(&g, NEAR_DUP, None);

		let after = stored(&g, &sid);
		assert_eq!(
			after.valid_until,
			Some(hour),
			"min(t, ∞) = t — omitting retention is no opinion, not 'make this permanent'"
		);
		assert_eq!(after.valid_until_lamport, before.valid_until_lamport);
		assert_eq!(
			after.valid_until_producer, before.valid_until_producer,
			"an unchanged deadline re-stamps nothing"
		);
	}

	#[test]
	fn a_tightening_dedup_stamps_the_survivor_only() {
		let deadline = SystemTime::now() + std::time::Duration::from_secs(3600);
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, None);

		ingest_chunk(&g, NEAR_DUP, Some(deadline));

		let ids = valid_until_stamped_ids(&g);
		assert_eq!(
			ids,
			vec![util::content_hash(SURVIVOR)],
			"exactly one ValidUntil stamp, on the survivor"
		);
		assert!(
			!ids.contains(&util::content_hash(NEAR_DUP)),
			"nothing stamped for the id that never entered the graph"
		);
	}

	#[test]
	fn the_second_dedup_gate_tightens_too_and_orphans_no_stamp() {
		let deadline = SystemTime::now() + std::time::Duration::from_secs(3600);
		let g = empty_graph();
		ingest_chunk(&g, SURVIVOR, None);
		let sid = util::content_hash(SURVIVOR);

		hide_from_gate_one(&g, &sid);

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
			valid_until_stamped_ids(&g),
			vec![sid],
			"one stamp, on the survivor — never the discarded incoming id"
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

		let (url, _server) = test_support::spawn_http(fixed_vec_app()).await;
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
