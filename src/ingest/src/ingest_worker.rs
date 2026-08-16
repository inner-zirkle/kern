//! The ingest worker: a bounded in-RAM job queue ahead of the LLM/embed legs.
//! A full queue refuses the job back to the producer (counted), and every
//! commit funnels through the same accept/dedup path as the durable legs.

use crate::ingest_place::{document_kind, place_chunks, place_document};
use base::base_types::*;
use graph::graph::GraphGnn;
use ingest_config::Config;
use llm::Client as LlmClient;
use math::clamp_confidence;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use util::LogThrottle;

use parking_lot::RwLock;
use tokio::sync::{mpsc, oneshot};

pub(crate) struct Job {
	pub(crate) text: String,
	pub(crate) source: Source,
	pub(crate) kind: EntityKind,
	pub(crate) hint: String,
	pub(crate) confidence: f64,
	pub(crate) config: Config,
	// Resolved from `config.review_policy` against `source`, once, at the gate.
	pub(crate) review: ReviewState,
	// The old-path external_id a `Renamed` file-event replaces; `None` for
	// ordinary ingests. `place_document` supersedes the entity that owns it so a
	// move-plus-edit does not leave a dangling stale `Document` (ROADMAP item 84).
	pub(crate) replaces: Option<String>,
	pub(crate) result_tx: Option<oneshot::Sender<Outcome>>,
	// Multi-tenancy scoping. None = global.
	pub(crate) scoping: Scoping,
}

// The ONLY place a Job is built, so `source_tag` is the one gate every producer
// passes. The clamp lives here rather than at each producer because a producer
// that forgot it is exactly the defect this closes (ROADMAP item 95): the file
// watcher minted `1.0`, a posterior of 0.6667 — a human's, and above the 0.6500
// a deliberate agent assertion gets.
#[allow(clippy::too_many_arguments)]
fn job(
	text: String,
	source: Source,
	kind: EntityKind,
	hint: String,
	confidence: f64,
	source_tag: &str,
	config: Config,
	replaces: Option<String>,
	result_tx: Option<oneshot::Sender<Outcome>>,
	scoping: Scoping,
) -> Job {
	// The confidence only. `kind` stays the producer's: a watched file is a
	// Document at 0.95, not the Claim the clamp's own classification would name.
	let (confidence, _) = clamp_confidence(confidence, source_tag);
	// Here for the same reason as the clamp: a producer that resolved its own
	// review state, or forgot to, is the defect. The scheme is only knowable per
	// job, so the policy travels and the resolution happens once, here.
	let review = crate::ingest::review_for(&config.review_policy, &source);
	Job {
		text,
		source,
		kind,
		hint,
		confidence,
		config,
		review,
		replaces,
		result_tx,
		scoping,
	}
}

// Runs on the commit path — must be cheap (enqueue only).
pub type DeferQuestionsFn = Arc<dyn Fn(&str) + Send + Sync>;

// Args are (kern_id, rephrase_reason_id); no hook = fail open.
pub type DeferContradictionFn = Arc<dyn Fn(&str, &str) + Send + Sync>;

// In-flight jobs the distill/embed leg may be behind on. The bound is the whole
// bound: nothing detaches past it.
const QUEUE_CAP: usize = 64;
const REFUSED_WARN_SECS: u64 = 60;
static QUEUE_REFUSED: AtomicU64 = AtomicU64::new(0);
static REFUSED_WARN: LogThrottle = LogThrottle::new(REFUSED_WARN_SECS);

// Jobs `enqueue` refused because the queue was full. The refusal is returned to
// the caller, but only the count says how often a producer outran the LLM leg.
pub fn ingest_queue_refused() -> u64 {
	QUEUE_REFUSED.load(Ordering::Relaxed)
}

// Writes the strict hygiene gate refused. A log is not a signal an operator
// can poll — the counter says whether the gate is eating a producer's output.
static HYGIENE_REJECTED: AtomicU64 = AtomicU64::new(0);

pub fn ingest_hygiene_rejected() -> u64 {
	HYGIENE_REJECTED.load(Ordering::Relaxed)
}

// `QUEUE_REFUSED` is process-global, so every test that fills a queue and
// asserts on the counter must hold this lock, or a parallel sibling's refusals
// land in its delta.
pub fn queue_refused_test_lock() -> &'static tokio::sync::Mutex<()> {
	static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
	LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

pub struct Worker {
	tx: mpsc::Sender<Job>,
}

impl Worker {
	pub fn new(
		graph: Arc<RwLock<GraphGnn>>,
		embedder: LlmClient,
		defer_questions: Option<DeferQuestionsFn>,
		defer_contradiction: Option<DeferContradictionFn>,
		save_fn: Option<Arc<dyn Fn() + Send + Sync>>,
	) -> Self {
		let (tx, rx) = mpsc::channel(QUEUE_CAP);
		tokio::spawn(run_loop(
			graph,
			embedder,
			defer_questions,
			defer_contradiction,
			save_fn,
			rx,
		));
		Self { tx }
	}

	// `None` = refused, queue full. A synchronous producer cannot wait on the LLM
	// leg without becoming as slow as it, and there is no oldest job worth
	// discarding for a newer one, so the newest is refused and the caller decides.
	#[allow(clippy::too_many_arguments)]
	pub fn enqueue(
		&self,
		text: String,
		source: Source,
		kind: EntityKind,
		hint: String,
		confidence: f64,
		source_tag: &str,
		config: Config,
		scoping: Scoping,
	) -> Option<String> {
		let doc_id = util::content_hash(&text);
		if self
			.tx
			.try_send(job(
				text, source, kind, hint, confidence, source_tag, config, None, None, scoping,
			))
			.is_err()
		{
			let total = QUEUE_REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
			if REFUSED_WARN.allow() {
				tracing::warn!(
					target: "kern.ingest",
					cap = QUEUE_CAP,
					total_refused = total,
					"ingest queue full; refusing the job (further refusals counted, not logged)"
				);
			}
			return None;
		}
		Some(doc_id)
	}

	// Jobs parked in the channel right now — the fill of the bound above. The
	// gauge beside `ingest_queue_refused`'s counter: the refusals say the bound
	// was hit, the depth says how close it is. A job the run loop has taken in
	// flight releases its slot and is not counted.
	pub fn queue_depth(&self) -> u64 {
		(self.tx.max_capacity() - self.tx.capacity()) as u64
	}

	// The waiting form of `enqueue`, for a producer that can be slowed instead of
	// refused. The file watcher is one: nothing is waiting on it, and its backlog
	// is coalesced paths rather than job bodies, so stalling it is cheaper than
	// losing a file that nothing will re-offer.
	#[allow(clippy::too_many_arguments)]
	pub async fn submit(
		&self,
		text: String,
		source: Source,
		kind: EntityKind,
		hint: String,
		confidence: f64,
		source_tag: &str,
		config: Config,
		replaces: Option<String>,
		scoping: Scoping,
	) -> Option<String> {
		let doc_id = util::content_hash(&text);
		let job = job(
			text, source, kind, hint, confidence, source_tag, config, replaces, None, scoping,
		);
		self.tx.send(job).await.ok().map(|()| doc_id)
	}

	#[allow(clippy::too_many_arguments)]
	pub async fn run(
		&self,
		text: String,
		source: Source,
		kind: EntityKind,
		hint: String,
		confidence: f64,
		source_tag: &str,
		config: Config,
		scoping: Scoping,
	) -> Outcome {
		let (result_tx, result_rx) = oneshot::channel();
		let job = job(
			text,
			source,
			kind,
			hint,
			confidence,
			source_tag,
			config,
			None,
			Some(result_tx),
			scoping,
		);
		if let Err(e) = self.tx.send(job).await {
			return Outcome::failed(
				"failed to enqueue",
				vec![FailureReport::document_permanent(format!(
					"send failed: {e}"
				))],
			);
		}
		result_rx
			.await
			.unwrap_or_else(|_| Outcome::failed("worker dropped", Vec::new()))
	}
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
	graph: Arc<RwLock<GraphGnn>>,
	embedder: LlmClient,
	defer_questions: Option<DeferQuestionsFn>,
	defer_contradiction: Option<DeferContradictionFn>,
	save_fn: Option<Arc<dyn Fn() + Send + Sync>>,
	mut rx: mpsc::Receiver<Job>,
) {
	while let Some(job) = rx.recv().await {
		let outcome = process(
			&graph,
			&embedder,
			&defer_questions,
			&defer_contradiction,
			&job,
		)
		.await;
		log_outcome(&outcome);
		if let Some(sf) = &save_fn {
			sf();
		}
		if let Some(tx) = job.result_tx {
			let _ = tx.send(outcome);
		}
	}
}

fn outcome_log_severity(o: &Outcome) -> &'static str {
	match o.status {
		OutcomeStatus::Failed => "error",
		OutcomeStatus::Partial => "warn",
		OutcomeStatus::Committed | OutcomeStatus::Deduped | OutcomeStatus::Rejected => "info",
	}
}

// Chunks a dead or failing embed endpoint cost us. A `Failed` job is logged at
// error level, but a log is not a signal an operator can poll — and until now
// nothing distinguished "the graph is empty because nothing was written" from
// "the graph is empty because every write was dropped" (ROADMAP item 7).
static INGEST_DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn ingest_dropped_chunks() -> u64 {
	INGEST_DROPPED.load(Ordering::Relaxed)
}

fn log_outcome(o: &Outcome) {
	if o.status == OutcomeStatus::Rejected {
		// The strict gate doing its job — noteworthy, not an error.
		tracing::info!(
			target: "kern.ingest",
			doc_id = %o.doc_id,
			message = %o.message,
			"ingest job refused by the hygiene gate"
		);
		return;
	}
	if o.failed_chunks > 0 {
		INGEST_DROPPED.fetch_add(o.failed_chunks as u64, Ordering::Relaxed);
	}
	let first_failure = o
		.failures
		.first()
		.map(|f| format!("{}/{}: {}", f.scope, f.class, f.error))
		.unwrap_or_default();
	match outcome_log_severity(o) {
		"error" => tracing::error!(
			target: "kern.ingest",
			doc_id = %o.doc_id,
			status = o.status.as_str(),
			total = o.total_chunks,
			embedded = o.embedded_chunks,
			failed = o.failed_chunks,
			first_failure = %first_failure,
			"ingest job failed"
		),
		"warn" => tracing::warn!(
			target: "kern.ingest",
			doc_id = %o.doc_id,
			status = o.status.as_str(),
			total = o.total_chunks,
			embedded = o.embedded_chunks,
			failed = o.failed_chunks,
			first_failure = %first_failure,
			"ingest job partially committed"
		),
		_ => tracing::info!(
			target: "kern.ingest",
			doc_id = %o.doc_id,
			status = o.status.as_str(),
			total = o.total_chunks,
			embedded = o.embedded_chunks,
			"ingest job committed"
		),
	}
}

// After a merge the acked content hash is not in the graph — carry the SURVIVING id.
fn finalize_doc_identity(
	content_id: &str,
	surviving_id: String,
	status: OutcomeStatus,
) -> (String, OutcomeStatus) {
	let deduped = surviving_id != content_id;
	let status = if deduped && status == OutcomeStatus::Committed {
		OutcomeStatus::Deduped
	} else {
		status
	};
	(surviving_id, status)
}

async fn process(
	graph: &Arc<RwLock<GraphGnn>>,
	embedder: &LlmClient,
	defer_questions: &Option<DeferQuestionsFn>,
	defer_contradiction: &Option<DeferContradictionFn>,
	job: &Job,
) -> Outcome {
	let doc_id = util::content_hash(&job.text);

	// The hygiene gate, before any embed spend: a refused write is `Rejected`,
	// not `Failed` — the durable legs archive it instead of retrying forever,
	// because a deterministic classifier refusing the same bytes cannot succeed
	// on retry. Warn mode logs what strict would refuse and proceeds.
	match hygiene::gate_write(&job.text, &job.config.hygiene) {
		hygiene::GateDecision::Reject(rej) => {
			HYGIENE_REJECTED.fetch_add(1, Ordering::Relaxed);
			return Outcome {
				status: OutcomeStatus::Rejected,
				doc_id,
				total_chunks: 0,
				embedded_chunks: 0,
				failed_chunks: 0,
				transient_failures: 0,
				permanent_failures: 0,
				failures: Vec::new(),
				message: format!("hygiene gate refused: {}", rej.reason),
			};
		}
		hygiene::GateDecision::Warn(rej) => {
			tracing::warn!(
				target: "kern.ingest",
				doc_id = %doc_id,
				reason = %rej.reason,
				"hygiene gate (warn) would refuse this ingest"
			);
		}
		hygiene::GateDecision::Allow => {}
	}

	// Heuristic split ONLY — an LLM split would add a per-document LLM call on the commit path.
	let chunks = crate::ingest_worker::split(&job.text, &job.hint, None);

	let (doc_thought, doc_fail) = place_document(
		graph,
		embedder,
		job,
		&doc_id,
		job.config.dedup_threshold_for(document_kind(job).0),
		defer_contradiction.as_ref(),
	)
	.await;
	let Some(surviving_id) = doc_thought else {
		let fail = doc_fail.unwrap_or_else(|| FailureReport::document_permanent("unknown"));
		return Outcome {
			status: OutcomeStatus::Failed,
			doc_id,
			total_chunks: chunks.len(),
			embedded_chunks: 0,
			failed_chunks: chunks.len(),
			transient_failures: if fail.class == "transient" { 1 } else { 0 },
			permanent_failures: if fail.class != "transient" { 1 } else { 0 },
			failures: vec![fail],
			message: "document embedding failed".into(),
		};
	};

	let (chunk_vecs, failures) = embed_chunks(embedder, &chunks).await;

	let placed = place_chunks(
		graph,
		defer_questions.as_ref(),
		defer_contradiction.as_ref(),
		job,
		&chunks,
		&chunk_vecs,
		&doc_id,
		job.config.dedup_threshold_for(job.kind),
	);

	let embedded_chunks = chunk_vecs.iter().filter(|v| !v.is_empty()).count();
	let failed_chunks = chunks.len() - embedded_chunks;
	let transient = failures.iter().filter(|f| f.class == "transient").count();
	let permanent = failures.iter().filter(|f| f.class != "transient").count();

	let status = classify_status(embedded_chunks, failed_chunks);
	let (doc_id, status) = finalize_doc_identity(&doc_id, surviving_id, status);

	Outcome {
		status,
		doc_id,
		total_chunks: chunks.len(),
		embedded_chunks,
		failed_chunks,
		transient_failures: transient,
		permanent_failures: permanent,
		failures,
		message: format!("{placed} chunks placed"),
	}
}

fn classify_status(embedded_chunks: usize, failed_chunks: usize) -> OutcomeStatus {
	if failed_chunks == 0 {
		OutcomeStatus::Committed
	} else if embedded_chunks > 0 {
		OutcomeStatus::Partial
	} else {
		OutcomeStatus::Failed
	}
}

// ==== [outcome] ====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeStatus {
	Committed,
	Partial,
	Deduped,
	Failed,
	// The hygiene gate refused the write. Distinct from `Failed` because the
	// durable legs retry `Failed` forever, and a deterministic refusal of the
	// same bytes can never succeed on retry — `Rejected` archives instead.
	Rejected,
}

impl OutcomeStatus {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Committed => "committed",
			Self::Partial => "partial",
			Self::Deduped => "deduped",
			Self::Failed => "failed",
			Self::Rejected => "rejected",
		}
	}
}

// class: "permanent" | "transient" (retryable); chunk_index 0 = document scope.
#[derive(Debug, Clone)]
pub struct FailureReport {
	pub scope: String,
	pub chunk_index: usize,
	pub class: String,
	pub error: String,
}

impl FailureReport {
	pub fn document_permanent(error: impl Into<String>) -> Self {
		Self {
			scope: "document".into(),
			chunk_index: 0,
			class: "permanent".into(),
			error: error.into(),
		}
	}
}

// INVARIANT: transient_failures + permanent_failures == failures.len().
#[derive(Debug, Clone)]
pub struct Outcome {
	pub status: OutcomeStatus,
	pub doc_id: String,
	pub total_chunks: usize,
	pub embedded_chunks: usize,
	pub failed_chunks: usize,
	pub transient_failures: usize,
	pub permanent_failures: usize,
	pub failures: Vec<FailureReport>,
	pub message: String,
}

impl Outcome {
	pub fn failed(message: impl Into<String>, failures: Vec<FailureReport>) -> Self {
		Self {
			status: OutcomeStatus::Failed,
			doc_id: String::new(),
			total_chunks: 0,
			embedded_chunks: 0,
			failed_chunks: 0,
			transient_failures: 0,
			permanent_failures: 0,
			failures,
			message: message.into(),
		}
	}
}

// ==== [embed] ====

use llm::is_transient;

const RETRY_DELAYS_MS: [u64; 3] = [150, 300, 600];

pub(crate) async fn embed_chunks(
	embedder: &LlmClient,
	chunks: &[String],
) -> (Vec<Vec<f32>>, Vec<FailureReport>) {
	if chunks.is_empty() {
		return (Vec::new(), Vec::new());
	}

	if let Ok(vecs) = embedder.embed_batch(chunks).await {
		if vecs.len() == chunks.len() {
			return (vecs, Vec::new());
		}
	}

	let mut vecs = Vec::with_capacity(chunks.len());
	let mut failures = Vec::new();
	for (i, chunk) in chunks.iter().enumerate() {
		match embed_with_retry(embedder, chunk, "chunk", i).await {
			Ok(v) => vecs.push(v),
			Err(fail) => {
				failures.push(fail);
				vecs.push(Vec::new());
			}
		}
	}
	(vecs, failures)
}

pub(crate) async fn embed_with_retry(
	embedder: &LlmClient,
	text: &str,
	scope: &str,
	chunk_index: usize,
) -> Result<Vec<f32>, FailureReport> {
	let mut last_err = None;

	for delay_ms in RETRY_DELAYS_MS.iter() {
		match embedder.embed(text).await {
			Ok(v) => return Ok(v),
			Err(e) => {
				if !is_transient(&e) {
					return Err(FailureReport {
						scope: scope.into(),
						chunk_index,
						class: "permanent".into(),
						error: e.to_string(),
					});
				}
				last_err = Some(e);
				tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
			}
		}
	}

	Err(FailureReport {
		scope: scope.into(),
		chunk_index,
		class: "transient".into(),
		error: last_err.map(|e| e.to_string()).unwrap_or_default(),
	})
}

// ==== [split] ====

pub fn split(text: &str, hint: &str, llm: Option<&dyn Fn(&str) -> String>) -> Vec<String> {
	if let Some(llm_fn) = llm {
		let result = llm_split(text, hint, llm_fn);
		if !result.is_empty() {
			return result;
		}
	}
	paragraph_split(text)
}

pub(crate) fn llm_split(text: &str, hint: &str, llm: &dyn Fn(&str) -> String) -> Vec<String> {
	let context = if hint.is_empty() {
		String::new()
	} else {
		format!(" This text describes {hint}.")
	};
	let prompt = format!(
		"Extract the key factual statements from the following text.{context} \
		 One statement per line. No numbering. No commentary.\n\n{text}"
	);
	let response = llm(&prompt);
	if response.is_empty() {
		return Vec::new();
	}
	trim_nonempty(response.lines())
}

pub(crate) fn paragraph_split(text: &str) -> Vec<String> {
	let chunks = trim_nonempty(text.split("\n\n"));
	if !chunks.is_empty() {
		return chunks;
	}
	let trimmed = text.trim();
	if trimmed.is_empty() {
		Vec::new()
	} else {
		vec![trimmed.to_string()]
	}
}

fn trim_nonempty<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<String> {
	parts
		.map(|p| p.trim().to_string())
		.filter(|p| !p.is_empty())
		.collect()
}

#[cfg(test)]
#[path = "tests/ingest_worker_test.rs"]
mod ingest_worker_tests;
