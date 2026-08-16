//! Tests extracted from ingest_intake.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use std::time::{Duration, SystemTime};
	use tempfile::tempdir;

	#[test]
	fn prune_done_removes_entries_older_than_retention() {
		let dir = tempdir().unwrap();
		let done = dir.path().to_path_buf();
		let f = done.join("old.txt");
		std::fs::write(&f, "x").unwrap();
		let future = SystemTime::now() + Duration::from_secs(3600);
		let removed = prune_done(&done, Duration::from_secs(60), future);
		assert_eq!(removed, 1);
		assert!(!f.exists());
	}

	#[test]
	fn prune_done_keeps_recent_entries() {
		let dir = tempdir().unwrap();
		let done = dir.path().to_path_buf();
		let f = done.join("fresh.txt");
		std::fs::write(&f, "x").unwrap();
		let removed = prune_done(&done, Duration::from_secs(3600), SystemTime::now());
		assert_eq!(removed, 0);
		assert!(f.exists());
	}

	#[test]
	fn prune_done_missing_dir_is_noop() {
		let dir = tempdir().unwrap();
		let missing = dir.path().join("nope");
		assert_eq!(
			prune_done(&missing, Duration::from_secs(1), SystemTime::now()),
			0
		);
	}

	fn stub_two(_q: &str) -> String {
		r#"[{"text":"fact one","kind":"fact"},{"text":"a preference","kind":"preference"}]"#.to_string()
	}

	#[test]
	fn extract_reads_and_distills() {
		let dir = tempdir().unwrap();
		let delta = dir.path().join("sess-1.txt");
		std::fs::write(&delta, "user: hi\nassistant: here is a fact").unwrap();
		let (stem, claims) = extract_claims(&delta, &[], &stub_two).expect("some");
		assert_eq!(stem, "sess-1");
		assert_eq!(claims.len(), 2);
	}

	#[test]
	fn extract_missing_file_is_none() {
		let dir = tempdir().unwrap();
		let missing = dir.path().join("nope.txt");
		assert!(extract_claims(&missing, &[], &stub_two).is_none());
	}

	#[test]
	fn extract_returns_none_on_llm_outage() {
		let dir = tempdir().unwrap();
		let delta = dir.path().join("sess-outage.txt");
		std::fs::write(&delta, "user: remember my API key lives in vault X").unwrap();
		let down = |_q: &str| String::new();
		assert!(extract_claims(&delta, &[], &down).is_none());
		assert!(delta.exists(), "delta must remain for retry after outage");
	}

	#[test]
	fn extract_returns_some_on_genuine_no_claims() {
		let dir = tempdir().unwrap();
		let delta = dir.path().join("sess-empty.txt");
		std::fs::write(&delta, "user: hi\nassistant: hello").unwrap();
		let nothing = |_q: &str| "[]".to_string();
		let (stem, claims) = extract_claims(&delta, &[], &nothing).expect("some");
		assert_eq!(stem, "sess-empty");
		assert!(claims.is_empty());
	}

	#[test]
	fn finalize_archives_when_all_ok() {
		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		let done = intake.join("done");
		let delta = intake.join("sess-1.txt");
		std::fs::write(&delta, "x").unwrap();
		assert!(finalize(&delta, &done, &[true, true]));
		assert!(!delta.exists());
		assert!(done.join("sess-1.txt").exists());
	}

	#[test]
	fn finalize_archives_when_no_claims() {
		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		let done = intake.join("done");
		let delta = intake.join("sess-2.txt");
		std::fs::write(&delta, "x").unwrap();
		assert!(finalize(&delta, &done, &[]));
		assert!(done.join("sess-2.txt").exists());
	}

	#[test]
	fn finalize_skips_archive_when_any_fail() {
		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		let done = intake.join("done");
		let delta = intake.join("sess-3.txt");
		std::fs::write(&delta, "x").unwrap();
		assert!(!finalize(&delta, &done, &[true, false]));
		assert!(delta.exists(), "delta left in intake for retry");
		assert!(!done.join("sess-3.txt").exists());
	}

	#[tokio::test]
	async fn drain_once_ingests_a_delta_and_archives_it_end_to_end() {
		use graph::graph::GraphGnn;
		use parking_lot::RwLock;

		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|_b: axum::Json<serde_json::Value>| async move {
				axum::Json(serde_json::json!({ "embeddings": [[0.1, 0.2, 0.3]] }))
			}),
		);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

		let embedder = llm::Client::new_embed_only(&format!("http://{addr}"), "m", "");
		let llm: LlmFunc =
			Arc::new(|_p: &str| r#"[{"text":"the API key lives in vault X","kind":"fact"}]"#.to_string());
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = Arc::new(Worker::new(graph.clone(), embedder, None, None, None));

		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		let done = intake.join("done");
		let delta = intake.join("sess-42.txt");
		std::fs::write(&delta, "user: where is my key\nassistant: vault X").unwrap();

		let cfg = crate::ingest::Config {
			dedup_threshold: 0.95,
			..Default::default()
		};
		let archived = drain_once(
			&intake,
			&done,
			&worker,
			Some(&llm),
			&[],
			&cfg,
			Duration::from_secs(3600),
			SystemTime::now(),
		)
		.await;

		assert_eq!(
			archived, 1,
			"the delta's single claim committed -> archived"
		);
		assert!(!delta.exists(), "consumed delta left the intake");
		assert!(done.join("sess-42.txt").exists(), "delta moved into done/");
		let g = graph.read();
		let entities: usize = g.all().iter().map(|k| k.entities.len()).sum();
		assert!(
			entities > 0,
			"the claim flowed through the worker into the graph"
		);

		server.abort();
	}

	// The intake promise: drop a document in, it lands — no reason LLM, no .txt suffix.
	#[tokio::test]
	async fn drain_once_ingests_a_non_txt_document_without_an_llm() {
		use graph::graph::GraphGnn;
		use parking_lot::RwLock;

		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|_b: axum::Json<serde_json::Value>| async move {
				axum::Json(serde_json::json!({ "embeddings": [[0.1, 0.2, 0.3]] }))
			}),
		);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

		let embedder = llm::Client::new_embed_only(&format!("http://{addr}"), "m", "");
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = Arc::new(Worker::new(graph.clone(), embedder, None, None, None));

		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		let done = intake.join("done");
		let doc = intake.join("spec.md");
		std::fs::write(&doc, "# Spec\n\nThe retry budget is four attempts.").unwrap();
		let binary = intake.join("logo.png");
		std::fs::write(&binary, [0xff, 0xd8, 0xff, 0xe0, 0x00]).unwrap();

		let cfg = crate::ingest::Config {
			dedup_threshold: 0.95,
			..Default::default()
		};
		let archived = drain_once(
			&intake,
			&done,
			&worker,
			None,
			&[],
			&cfg,
			Duration::from_secs(3600),
			SystemTime::now(),
		)
		.await;

		assert_eq!(archived, 1, "the document committed with no LLM configured");
		assert!(!doc.exists(), "consumed document left the intake");
		assert!(done.join("spec.md").exists(), "document moved into done/");
		assert!(
			!binary.exists() && intake.join("failed").join("logo.png").exists(),
			"binary quarantined into failed/ instead of sitting in the intake forever"
		);
		let g = graph.read();
		let entities: usize = g.all().iter().map(|k| k.entities.len()).sum();
		assert!(entities > 0, "the document reached the graph");

		server.abort();
	}

	// The `.txt` distillation path used to build its per-claim config from the
	// queue's config and then overwrite only `valid_from`, so a queue with a
	// standing retention policy still produced claims that never expire.
	#[tokio::test]
	async fn a_queue_retention_reaches_the_distilled_claim() {
		use graph::graph::GraphGnn;
		use parking_lot::RwLock;

		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|_b: axum::Json<serde_json::Value>| async move {
				axum::Json(serde_json::json!({ "embeddings": [[0.1, 0.2, 0.3]] }))
			}),
		);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

		let embedder = llm::Client::new_embed_only(&format!("http://{addr}"), "m", "");
		let llm: LlmFunc =
			Arc::new(|_p: &str| r#"[{"text":"the pager rotation is Ada's","kind":"fact"}]"#.to_string());
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = Arc::new(Worker::new(graph.clone(), embedder, None, None, None));

		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		let delta = intake.join("sess-ttl.txt");
		std::fs::write(&delta, "user: who is oncall\nassistant: Ada").unwrap();

		let deadline = SystemTime::now() + Duration::from_secs(3600);
		let cfg = crate::ingest::Config {
			valid_until: Some(deadline),
			..Default::default()
		};
		assert!(
			drain_entry(
				&delta,
				&intake.join("done"),
				&intake.join("failed"),
				&worker,
				Some(&llm),
				&[],
				&cfg,
			)
			.await,
			"the transcript committed"
		);

		let g = graph.read();
		let deadlines: Vec<Option<SystemTime>> = g
			.all()
			.iter()
			.flat_map(|k| k.entities.values().map(|e| e.valid_until))
			.collect();
		assert!(!deadlines.is_empty(), "the claim reached the graph");
		assert!(
			deadlines.iter().all(|v| *v == Some(deadline)),
			"every distilled claim carries the queue's deadline, got {deadlines:?}"
		);

		server.abort();
	}

	// Where `with_retention` is called matters more than what it returns, and no
	// test of the conversion itself can see that. Built once above the loop —
	// where this config lived until now — a daemon would hand every transcript
	// it ever sees a deadline measured from boot, so a queue configured for 30
	// days would expire month-old and minute-old deltas at the same instant.
	// Two passes a beat apart must therefore stamp two different deadlines.
	#[tokio::test]
	async fn the_poll_loop_resolves_its_deadline_per_pass_not_once_at_startup() {
		use graph::graph::GraphGnn;
		use parking_lot::RwLock;

		// Distinct vectors per text: a constant embedding makes the second claim
		// a near-duplicate of the first, and it never lands as its own entity.
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|b: axum::Json<serde_json::Value>| async move {
				let v = if b.0.to_string().contains("alpha") {
					[1.0, 0.0, 0.0]
				} else {
					[0.0, 1.0, 0.0]
				};
				axum::Json(serde_json::json!({ "embeddings": [v] }))
			}),
		);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

		let embedder = llm::Client::new_embed_only(&format!("http://{addr}"), "m", "");
		let llm: LlmFunc = Arc::new(|p: &str| {
			let which = if p.contains("alpha") { "alpha" } else { "beta" };
			format!(r#"[{{"text":"the {which} rotation is Ada's","kind":"fact"}}]"#)
		});
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = Arc::new(Worker::new(graph.clone(), embedder, None, None, None));

		// Distinct `valid_until`s, polled: the drain is a background loop, so the
		// commit is observed rather than awaited.
		async fn deadlines_reaching(g: &Arc<RwLock<GraphGnn>>, n: usize) -> Vec<SystemTime> {
			let cap = std::time::Instant::now() + Duration::from_secs(10);
			loop {
				let mut got: Vec<SystemTime> = g
					.read()
					.all()
					.iter()
					.flat_map(|k| k.entities.values().filter_map(|e| e.valid_until))
					.collect();
				got.sort();
				got.dedup();
				if got.len() >= n || std::time::Instant::now() > cap {
					return got;
				}
				tokio::time::sleep(Duration::from_millis(20)).await;
			}
		}

		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		std::fs::write(intake.join("a.txt"), "user: q\nassistant: alpha").unwrap();

		let drain = tokio::spawn(run(
			intake.clone(),
			worker.clone(),
			Some(llm),
			None,
			0.9,
			3600,
			Default::default(),
			Default::default(),
			Duration::from_millis(50),
			Duration::from_secs(3600),
		));

		let first = deadlines_reaching(&graph, 1).await;
		assert_eq!(
			first.len(),
			1,
			"the first transcript's claim reached the graph"
		);

		// Wait on the wall clock, not the monotonic one. `valid_until` is an
		// absolute `SystemTime`, and this box runs `CLOCK_REALTIME` ~3.8% slow
		// and steps it backwards by the accrued drift once per ~32s, so a
		// monotonic sleep of two seconds can advance realtime by less than one —
		// which reads as a deadline pinned at startup and fails a test about
		// something else entirely.
		let mut marker = SystemTime::now();
		let cap = std::time::Instant::now() + Duration::from_secs(30);
		loop {
			match SystemTime::now().duration_since(marker) {
				Ok(d) if d >= Duration::from_secs(2) => break,
				// The clock stepped backwards mid-wait. Restart from the new
				// reading: an `Err` here must not read as "the wait is over".
				Err(_) => marker = SystemTime::now(),
				Ok(_) => {}
			}
			assert!(
				std::time::Instant::now() < cap,
				"realtime never advanced two seconds in thirty monotonic ones"
			);
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
		std::fs::write(intake.join("b.txt"), "user: q\nassistant: beta").unwrap();
		let both = deadlines_reaching(&graph, 2).await;

		assert_eq!(both.len(), 2, "two passes, two deadlines — got {both:?}");
		let gap = both[1].duration_since(both[0]).unwrap();
		assert!(
			gap >= Duration::from_millis(500),
			"a transcript queued two seconds later expires later (gap was {gap:?}, \
			 need ≥500ms); a deadline built once at startup would make them equal"
		);

		drain.abort();
		server.abort();
	}

	// A delta retried forever with no sidecar reads exactly like one not yet
	// picked up. Every path that leaves a delta in the queue must say why, or
	// `kern intake` reports a permanently stuck transcript as merely waiting.
	#[tokio::test]
	async fn a_transcript_left_queued_records_why_it_is_stuck() {
		use crate::ingest_intake_status::{last_failure, scan};
		use graph::graph::GraphGnn;
		use parking_lot::RwLock;

		let embedder = llm::Client::new_embed_only("http://127.0.0.1:1", "m", "");
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = Arc::new(Worker::new(graph.clone(), embedder, None, None, None));

		let dir = tempdir().unwrap();
		let intake = dir.path().to_path_buf();
		let no_llm = intake.join("needs-distill.txt");
		std::fs::write(&no_llm, "user: hi\nassistant: a fact").unwrap();
		let prose = intake.join("prose-reply.txt");
		std::fs::write(&prose, "user: hi\nassistant: another fact").unwrap();

		let cfg = crate::ingest::Config::default();
		// No LLM at all: the transcript cannot even be attempted.
		drain_once(
			&intake,
			&intake.join("done"),
			&worker,
			None,
			&[],
			&cfg,
			Duration::from_secs(3600),
			SystemTime::now(),
		)
		.await;

		assert!(no_llm.exists(), "precondition: the delta is still queued");
		assert!(
			last_failure(&intake, "needs-distill.txt")
				.unwrap_or_default()
				.contains("[reason]"),
			"a transcript with no reason endpoint says so"
		);

		// An LLM that answers, but never in the parseable shape.
		let prose_llm: LlmFunc = Arc::new(|_p: &str| "Sure! Here are the facts:".to_string());
		drain_once(
			&intake,
			&intake.join("done"),
			&worker,
			Some(&prose_llm),
			&[],
			&cfg,
			Duration::from_secs(3600),
			SystemTime::now(),
		)
		.await;

		assert!(prose.exists(), "precondition: the delta is still queued");
		assert!(
			last_failure(&intake, "prose-reply.txt")
				.unwrap_or_default()
				.contains("no parseable claims"),
			"a prose-answering reason model says so"
		);

		let report = scan(&intake, SystemTime::now());
		assert_eq!(
			report.stuck(),
			2,
			"both are STUCK, not merely pending: {:?}",
			report.pending
		);
	}
}
