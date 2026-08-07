//! The intake queue consumer: poll the drop dir, distill `.txt` transcripts
//! into claims, ingest other readable files whole, archive what committed into
//! `done/` and record what failed — the durable half of ingest.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::ingest::Worker;
use crate::ingest_distill::{distill, Claim};
use crate::ingest_worker::OutcomeStatus;
use base::base_types::{EntityKind, Source};
use llm::LlmFunc;

pub type ClaimKindsFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

pub fn extract_claims(
	path: &Path,
	extra_kinds: &[String],
	llm: &dyn Fn(&str) -> String,
) -> Option<(String, Vec<Claim>)> {
	let text = match read_text(path)? {
		Text::Content(t) => t,
		Text::Binary => return None,
	};
	let stem = path
		.file_stem()
		.and_then(|s| s.to_str())
		.unwrap_or("session")
		.to_string();
	let claims = match distill(&text, extra_kinds, llm, std::time::SystemTime::now()) {
		Some(c) => c,
		None => {
			tracing::warn!(target: "kern.ingest.intake", path = %path.display(), "distill got no LLM output; leaving delta in intake for retry");
			return None;
		}
	};
	Some((stem, claims))
}

pub enum Text {
	Content(String),
	Binary,
}

// None = transient read error, retry next drain. Binary = never ingestable, quarantine.
fn read_text(path: &Path) -> Option<Text> {
	match std::fs::read_to_string(path) {
		Ok(t) => Some(Text::Content(t)),
		Err(e) if e.kind() == std::io::ErrorKind::InvalidData => Some(Text::Binary),
		Err(e) => {
			tracing::warn!(target: "kern.ingest.intake", path = %path.display(), error = %e, "failed to read intake file; leaving for retry");
			None
		}
	}
}

// Best effort: on rename failure (cross-device) the source is removed so it is not re-processed.
pub fn archive(path: &Path, done_dir: &Path) {
	if let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) {
		crate::ingest_intake_status::clear_failure(dir, name);
	}
	let _ = std::fs::create_dir_all(done_dir);
	if let Some(name) = path.file_name() {
		if std::fs::rename(path, done_dir.join(name)).is_err() {
			let _ = std::fs::remove_file(path);
		}
	}
}

// The queue dir is the file's parent; sidecars live in `<queue>/errors/`.
fn record_intake_failure(path: &Path, outcome: &crate::ingest_worker::Outcome) {
	let first = outcome
		.failures
		.first()
		.map(|f| format!("{}/{}: {}", f.scope, f.class, f.error))
		.unwrap_or_else(|| "no failure detail reported".to_string());
	record_stuck(
		path,
		&format!("status={} {}", outcome.status.as_str(), first),
	);
}

// Every path that leaves a delta queued must land here. A delta retried forever
// with no sidecar is indistinguishable from one not yet picked up, which is the
// invisibility `kern intake` exists to end.
fn record_stuck(path: &Path, message: &str) {
	let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
		return;
	};
	crate::ingest_intake_status::record_failure(dir, name, message);
}

pub fn finalize(path: &Path, done_dir: &Path, results: &[bool]) -> bool {
	if results.iter().all(|&ok| ok) {
		archive(path, done_dir);
		true
	} else {
		false
	}
}

pub fn prune_done(done_dir: &Path, max_age: Duration, now: SystemTime) -> usize {
	let entries = match std::fs::read_dir(done_dir) {
		Ok(e) => e,
		Err(_) => return 0,
	};
	let mut removed = 0;
	for ent in entries.flatten() {
		let path = ent.path();
		if !path.is_file() {
			continue;
		}
		let modified = match ent.metadata().and_then(|m| m.modified()) {
			Ok(m) => m,
			Err(_) => continue,
		};
		let too_old = now
			.duration_since(modified)
			.map(|age| age > max_age)
			.unwrap_or(false);
		if too_old && std::fs::remove_file(&path).is_ok() {
			removed += 1;
		}
	}
	removed
}

// The intake contract: anything readable as text gets in. `.txt` is a session
// transcript and is distilled into claims; everything else is a document and is
// stored whole, which is why documents need no reason LLM. Binary is quarantined
// rather than left to sit forever looking accepted.
// ponytail: a file still being copied can read as valid-but-truncated text; a
// mtime-settle check is the upgrade path if partial drops show up in practice.
async fn drain_entry(
	path: &Path,
	done: &Path,
	failed: &Path,
	worker: &Worker,
	llm: Option<&LlmFunc>,
	extra_kinds: &[String],
	cfg: &crate::ingest::Config,
) -> bool {
	if !path.is_file() {
		return false;
	}
	let text = match read_text(path) {
		Some(Text::Content(t)) => t,
		Some(Text::Binary) => {
			tracing::warn!(target: "kern.ingest.intake", path = %path.display(), "not text; moved to failed/");
			archive(path, failed);
			return false;
		}
		None => {
			record_stuck(
				path,
				"unreadable (transient IO error); left queued for retry",
			);
			return false;
		}
	};
	if text.trim().is_empty() {
		archive(path, done);
		return true;
	}
	if path.extension().and_then(|s| s.to_str()) != Some("txt") {
		return drain_document(path, &text, done, worker, cfg).await;
	}
	let Some(llm) = llm else {
		tracing::warn!(target: "kern.ingest.intake", path = %path.display(), "transcript needs a reason LLM to distill; leaving in intake");
		record_stuck(
			path,
			"no [reason] endpoint configured — a .txt transcript cannot be distilled",
		);
		return false;
	};
	let (stem, claims) = match extract_claims(path, extra_kinds, llm.as_ref()) {
		Some(v) => v,
		None => {
			record_stuck(
				path,
				"the reason model returned no parseable claims (prose reply, or endpoint unreachable)",
			);
			return false;
		}
	};
	let mut results = Vec::with_capacity(claims.len());
	for c in claims {
		// Turn-level provenance: which 1-based turns the distiller drew the claim
		// from, comma-joined into Source::Session.section. Empty when uncited,
		// matching the pre-provenance baseline.
		let section = if c.turns.is_empty() {
			String::new()
		} else {
			c.turns
				.iter()
				.map(|t| t.to_string())
				.collect::<Vec<_>>()
				.join(",")
		};
		let src = Source::Session {
			session_id: format!("session:{stem}"),
			section,
			title: format!("session://{}", c.kind),
		};
		// The clone carries the queue's standing `valid_until`; only the lower
		// bound is per-claim, because only that one the distiller can know.
		let mut claim_cfg = cfg.clone();
		claim_cfg.valid_from = c.valid_from;
		let tag = src.scheme();
		let outcome = worker
			.run(
				c.text,
				src,
				EntityKind::Claim,
				c.kind,
				0.6,
				tag,
				claim_cfg,
				base::base_types::Scoping::default(),
			)
			.await;
		let ok = !matches!(outcome.status, OutcomeStatus::Failed);
		if !ok {
			tracing::warn!(target: "kern.ingest.intake", stem = %stem, status = outcome.status.as_str(), "claim ingest failed; leaving delta for retry");
			record_intake_failure(path, &outcome);
		}
		results.push(ok);
	}
	finalize(path, done, &results)
}

async fn drain_document(
	path: &Path,
	text: &str,
	done: &Path,
	worker: &Worker,
	cfg: &crate::ingest::Config,
) -> bool {
	let name = path
		.file_name()
		.and_then(|s| s.to_str())
		.unwrap_or("document")
		.to_string();
	let src = Source::File {
		path: name.clone(),
		section: String::new(),
		title: name.clone(),
		author: String::new(),
		url: String::new(),
	};
	// Same channel as the watcher, and the same reason: a file dropped into the
	// intake asserted nothing. This path minted a raw 1.0 too.
	let tag = src.scheme();
	let outcome = worker
		.run(
			text.to_string(),
			src,
			EntityKind::Document,
			String::new(),
			1.0,
			tag,
			cfg.clone(),
			base::base_types::Scoping::default(),
		)
		.await;
	let ok = !matches!(outcome.status, OutcomeStatus::Failed);
	if !ok {
		tracing::warn!(target: "kern.ingest.intake", name = %name, status = outcome.status.as_str(), "document ingest failed; leaving in intake for retry");
		record_intake_failure(path, &outcome);
	}
	finalize(path, done, &[ok])
}

// One pass over the queue, for a CLI with no daemon running. The looping
// `run` below is the daemon's caller; both share `drain_once` so a one-shot
// drain can never diverge from what the daemon would have done.
#[allow(clippy::too_many_arguments)]
pub async fn drain_now(
	intake_dir: &Path,
	worker: &Worker,
	llm: Option<&LlmFunc>,
	extra_kinds: &[String],
	dedup_threshold: f64,
	retention_secs: u64,
	review_policy: crate::ingest::ReviewPolicy,
	done_retention: Duration,
	now: SystemTime,
) -> usize {
	let cfg = crate::ingest::Config {
		dedup_threshold,
		review_policy,
		..Default::default()
	}
	.with_retention(retention_secs);
	drain_once(
		intake_dir,
		&intake_dir.join("done"),
		worker,
		llm,
		extra_kinds,
		&cfg,
		done_retention,
		now,
	)
	.await
}

#[allow(clippy::too_many_arguments)]
async fn drain_once(
	intake_dir: &Path,
	done: &Path,
	worker: &Worker,
	llm: Option<&LlmFunc>,
	extra_kinds: &[String],
	cfg: &crate::ingest::Config,
	done_retention: Duration,
	now: SystemTime,
) -> usize {
	let entries = match std::fs::read_dir(intake_dir) {
		Ok(e) => e,
		Err(e) => {
			tracing::warn!(target: "kern.ingest.intake", dir = %intake_dir.display(), error = %e, "failed to read intake dir");
			return 0;
		}
	};
	let failed = intake_dir.join("failed");
	let mut archived = 0;
	for ent in entries.flatten() {
		if drain_entry(&ent.path(), done, &failed, worker, llm, extra_kinds, cfg).await {
			archived += 1;
		}
	}
	archived +=
		crate::ingest_direct::drain_direct_once(&intake_dir.join("direct"), worker, cfg).await;
	prune_done(done, done_retention, now);
	prune_done(&intake_dir.join("direct").join("done"), done_retention, now);
	archived
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
	intake_dir: PathBuf,
	worker: Arc<Worker>,
	llm: Option<LlmFunc>,
	claim_kinds: Option<ClaimKindsFn>,
	dedup_threshold: f64,
	retention_secs: u64,
	review_policy: crate::ingest::ReviewPolicy,
	interval: Duration,
	done_retention: Duration,
) {
	let _ = std::fs::create_dir_all(&intake_dir);
	let done = intake_dir.join("done");
	loop {
		// Per pass, not once above the loop: this daemon outlives its deltas, and
		// a deadline resolved at startup would give a transcript dropped a month
		// from now a TTL that already expired.
		let cfg = crate::ingest::Config {
			dedup_threshold,
			review_policy: review_policy.clone(),
			..Default::default()
		}
		.with_retention(retention_secs);
		let extra_kinds = claim_kinds.as_ref().map(|f| f()).unwrap_or_default();
		drain_once(
			&intake_dir,
			&done,
			&worker,
			llm.as_ref(),
			&extra_kinds,
			&cfg,
			done_retention,
			SystemTime::now(),
		)
		.await;
		tokio::time::sleep(interval).await;
	}
}

#[cfg(test)]
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
			gap >= Duration::from_secs(1),
			"a transcript queued two seconds later must expire two seconds later; \
			 the deadlines are {gap:?} apart, which is a config built once at startup"
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
