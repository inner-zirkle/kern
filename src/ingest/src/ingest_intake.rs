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
	hygiene: hygiene::GateConfig,
	done_retention: Duration,
	now: SystemTime,
) -> usize {
	let cfg = crate::ingest::Config {
		dedup_threshold,
		review_policy,
		hygiene,
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
	hygiene: hygiene::GateConfig,
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
			hygiene: hygiene.clone(),
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
#[path = "tests/ingest_intake_test.rs"]
mod ingest_intake_tests;
