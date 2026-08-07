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
use util;
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

// `QUEUE_REFUSED` is process-global, so every test that fills a queue and
// asserts on the counter must hold this lock, or a parallel sibling's refusals
// land in its delta.
#[cfg(test)]
pub(crate) fn queue_refused_test_lock() -> &'static tokio::sync::Mutex<()> {
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
		OutcomeStatus::Committed | OutcomeStatus::Deduped => "info",
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
}

impl OutcomeStatus {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Committed => "committed",
			Self::Partial => "partial",
			Self::Deduped => "deduped",
			Self::Failed => "failed",
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
mod tests {
	use super::*;

	#[test]
	fn classify_status_maps_tallies_to_outcome() {
		assert_eq!(classify_status(3, 0), OutcomeStatus::Committed);
		assert_eq!(classify_status(2, 1), OutcomeStatus::Partial);
		assert_eq!(classify_status(0, 3), OutcomeStatus::Failed);
		assert_eq!(classify_status(0, 0), OutcomeStatus::Committed);
	}

	#[test]
	fn document_permanent_failure_has_canonical_shape() {
		let f = FailureReport::document_permanent("graph lock poisoned");
		assert_eq!(f.scope, "document");
		assert_eq!(f.chunk_index, 0);
		assert_eq!(f.class, "permanent");
		assert_eq!(f.error, "graph lock poisoned");
	}

	fn session_source() -> Source {
		Source::Session {
			session_id: "s".into(),
			section: "sec".into(),
			title: String::new(),
		}
	}

	fn dead_worker(graph: Arc<RwLock<GraphGnn>>) -> Worker {
		let embedder = LlmClient::new_embed_only("http://127.0.0.1:1", "test", "");
		Worker::new(graph, embedder, None, None, None)
	}

	#[tokio::test]
	async fn enqueue_returns_the_content_hash_doc_id() {
		let worker = dead_worker(Arc::new(RwLock::new(GraphGnn::new())));
		let text = "some document text".to_string();
		let doc_id = worker.enqueue(
			text.clone(),
			session_source(),
			EntityKind::Claim,
			String::new(),
			1.0,
			"session",
			Config::default(),
			Scoping::default(),
		);
		assert_eq!(doc_id, Some(util::content_hash(&text)));
	}

	// The trap this test exists to avoid: offering fewer jobs than the bound
	// passes whether or not a bound exists. OFFERED is many times QUEUE_CAP, so
	// an unbounded `enqueue` accepts all 500 and fails both assertions.
	#[tokio::test]
	async fn enqueue_refuses_past_the_queue_bound_and_counts_the_refusal() {
		const OFFERED: usize = 500;

		let _serial = queue_refused_test_lock().lock().await;
		let (url, _server) =
			crate::test_support::spawn_http(crate::test_support::hanging_embed_app()).await;
		let embedder = LlmClient::new_embed_only(&url, "m", "");
		let worker = Worker::new(
			Arc::new(RwLock::new(GraphGnn::new())),
			embedder,
			None,
			None,
			None,
		);

		let before = ingest_queue_refused();
		let mut accepted = 0usize;
		for i in 0..OFFERED {
			let got = worker.enqueue(
				format!("document number {i}"),
				session_source(),
				EntityKind::Claim,
				String::new(),
				1.0,
				"session",
				Config::default(),
				Scoping::default(),
			);
			if got.is_some() {
				accepted += 1;
			}
			// Let the run loop take its one job and park on the hanging embedder.
			tokio::task::yield_now().await;
		}

		assert!(
			(QUEUE_CAP..=QUEUE_CAP + 1).contains(&accepted),
			"the bound bounds: at most the {QUEUE_CAP} queued plus the one in flight, accepted {accepted} of {OFFERED}"
		);
		assert_eq!(
			ingest_queue_refused() - before,
			(OFFERED - accepted) as u64,
			"every refusal is counted, or a full queue is a degradation nobody can see"
		);
	}

	// An embed endpoint gated on a semaphore: with no permits it stalls the
	// worker on its first job, and opening it drains everything.
	fn gated_embed_app(gate: Arc<tokio::sync::Semaphore>) -> axum::Router {
		axum::Router::new().route(
			"/api/embed",
			axum::routing::post(move |body: axum::Json<serde_json::Value>| {
				let gate = gate.clone();
				async move {
					gate.acquire().await.unwrap().forget();
					let n = body
						.0
						.get("input")
						.and_then(|v| v.as_array())
						.map(|a| a.len())
						.unwrap_or(1);
					let embs: Vec<Vec<f32>> = (0..n).map(|_| STUB_VEC.to_vec()).collect();
					axum::Json(serde_json::json!({ "embeddings": embs }))
				}
			}),
		)
	}

	async fn depth_settles_to_zero(worker: &Worker) -> bool {
		let cap = std::time::Instant::now() + std::time::Duration::from_secs(10);
		while worker.queue_depth() > 0 {
			if std::time::Instant::now() >= cap {
				return false;
			}
			tokio::time::sleep(std::time::Duration::from_millis(10)).await;
		}
		true
	}

	#[tokio::test]
	async fn queue_depth_reads_the_parked_jobs_and_falls_after_drain() {
		const PARKED: usize = 5;
		let gate = Arc::new(tokio::sync::Semaphore::new(0));
		let (url, _server) = crate::test_support::spawn_http(gated_embed_app(gate.clone())).await;
		let worker = Worker::new(
			Arc::new(RwLock::new(GraphGnn::new())),
			LlmClient::new_embed_only(&url, "m", ""),
			None,
			None,
			None,
		);
		assert_eq!(worker.queue_depth(), 0, "an idle worker parks nothing");

		// One job into flight, so the loop is parked on the gate rather than on
		// recv — from here nothing leaves the channel until the gate opens.
		worker.enqueue(
			"job 0".into(),
			session_source(),
			EntityKind::Claim,
			String::new(),
			1.0,
			"session",
			Config::default(),
			Scoping::default(),
		);
		assert!(
			depth_settles_to_zero(&worker).await,
			"the in-flight job must release its slot, or the gauge overcounts by one"
		);

		for i in 1..=PARKED {
			worker.enqueue(
				format!("job {i}"),
				session_source(),
				EntityKind::Claim,
				String::new(),
				1.0,
				"session",
				Config::default(),
				Scoping::default(),
			);
		}
		assert_eq!(
			worker.queue_depth(),
			PARKED as u64,
			"every parked job is counted, exactly"
		);

		gate.add_permits(10_000);
		assert!(
			depth_settles_to_zero(&worker).await,
			"the gauge must fall as the drain takes the parked jobs"
		);
	}

	fn outcome_with(status: OutcomeStatus) -> Outcome {
		Outcome {
			status,
			doc_id: "d".into(),
			total_chunks: 1,
			embedded_chunks: 1,
			failed_chunks: 0,
			transient_failures: 0,
			permanent_failures: 0,
			failures: Vec::new(),
			message: String::new(),
		}
	}

	#[test]
	fn finalize_doc_identity_marks_dedup_and_keeps_surviving_id() {
		let (id, st) =
			finalize_doc_identity("hash-a", "existing-b".to_string(), OutcomeStatus::Committed);
		assert_eq!(id, "existing-b");
		assert_eq!(st, OutcomeStatus::Deduped);

		let (id, st) = finalize_doc_identity("hash-a", "hash-a".to_string(), OutcomeStatus::Committed);
		assert_eq!(id, "hash-a");
		assert_eq!(st, OutcomeStatus::Committed);

		let (_, st) = finalize_doc_identity("hash-a", "existing-b".to_string(), OutcomeStatus::Partial);
		assert_eq!(st, OutcomeStatus::Partial);
	}

	#[test]
	fn outcome_log_severity_maps_status_to_level() {
		assert_eq!(
			outcome_log_severity(&outcome_with(OutcomeStatus::Committed)),
			"info"
		);
		assert_eq!(
			outcome_log_severity(&outcome_with(OutcomeStatus::Deduped)),
			"info"
		);
		assert_eq!(
			outcome_log_severity(&outcome_with(OutcomeStatus::Partial)),
			"warn"
		);
		assert_eq!(
			outcome_log_severity(&outcome_with(OutcomeStatus::Failed)),
			"error"
		);
	}

	const STUB_VEC: [f32; 3] = [0.1, 0.2, 0.3];

	fn job_for(text: &str) -> Job {
		Job {
			text: text.into(),
			source: session_source(),
			kind: EntityKind::Claim,
			hint: String::new(),
			confidence: 1.0,
			config: Config::default(),
			review: ReviewState::default(),
			replaces: None,
			result_tx: None,
			scoping: Scoping::default(),
		}
	}

	#[tokio::test]
	async fn a_second_gate_dedup_reports_deduped_and_the_surviving_id() {
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let (url, _server) =
			crate::test_support::spawn_http(crate::test_support::fixed_vec_embed_app()).await;
		let embedder = LlmClient::new_embed_only(&url, "m", "");

		let first = job_for("alpha beta gamma");
		let out = process(&graph, &embedder, &None, &None, &first).await;
		assert_eq!(out.status, OutcomeStatus::Committed, "the survivor lands");
		let sid = util::content_hash(&first.text);

		// Hide the survivor from `find_duplicate` (which reads entity_idx alone)
		// while leaving it visible to accept_with_dedup's wider scan — the exact
		// shape that walks past gate 1 into commit_entity's dup branch.
		{
			let mut g = graph.write();
			g.entity_idx.delete(&sid);
			g.gnn_entity_idx
				.insert(sid.clone(), STUB_VEC.to_vec().into());
			assert!(
				g.entity_idx.is_empty(),
				"fixture is only honest while gate 1 has nothing left to hit"
			);
		}

		let second = job_for("alpha beta gamma, restated");
		let out = process(&graph, &embedder, &None, &None, &second).await;

		assert_eq!(
			graph
				.read()
				.all()
				.iter()
				.map(|k| k.entities.len())
				.sum::<usize>(),
			1,
			"gate 2 dropped the incoming document"
		);
		assert_eq!(
			out.doc_id, sid,
			"the ack must name the survivor, not a content hash the graph never stored"
		);
		assert_eq!(
			out.status,
			OutcomeStatus::Deduped,
			"a second-gate dedup is a dedup — reporting Committed lies about what happened"
		);
	}

	// ROADMAP item 95. The clamp used to live at each producer, so a producer that
	// forgot it minted an unclamped confidence — which is how the file watcher
	// shipped a raw 1.0. The guard is now `job()`, and this enumerates EVERY
	// public entry point rather than the one that was caught: a new method that
	// builds a Job by hand is the same defect again, and this test is what a
	// reviewer runs to see it.
	#[test]
	fn no_entry_point_can_mint_an_unclamped_confidence() {
		let file = || Source::File {
			path: "p".into(),
			section: String::new(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		};
		let build = |conf: f64, tag: &str| {
			job(
				"t".into(),
				file(),
				EntityKind::Document,
				String::new(),
				conf,
				tag,
				Config::default(),
				None,
				None,
				Scoping::default(),
			)
		};

		assert_eq!(
			build(1.0, "file").confidence,
			base::base_constants::MAX_AI_CONFIDENCE,
			"a non-user channel is capped, whatever it asked for"
		);
		assert_eq!(
			build(1.0, base::base_constants::AGENT_SOURCE).confidence,
			base::base_constants::MAX_AI_CONFIDENCE,
		);
		assert_eq!(
			build(1.0, base::base_constants::USER_SOURCE).confidence,
			1.0,
			"the one path with a human behind it keeps its 1.0"
		);
		assert_eq!(
			build(base::base_constants::MAX_AI_CONFIDENCE, "file").confidence,
			base::base_constants::MAX_AI_CONFIDENCE,
			"idempotent: a producer that already clamped is not clamped twice"
		);
		assert_eq!(
			build(1.0, "file").kind,
			EntityKind::Document,
			"the clamp takes the confidence only — a watched file stays a Document, \
			 not the Claim clamp_confidence's own classification would name"
		);
	}

	// The guard above is only a guard if every entrance walks through it. This
	// drives the real methods against a real graph, so a future entrance that
	// builds a `Job` by hand fails here rather than shipping a 1.0 the way the
	// file watcher did.
	#[tokio::test]
	async fn every_public_entry_point_walks_through_the_clamp() {
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let (url, _server) =
			crate::test_support::spawn_http(crate::test_support::fixed_vec_embed_app()).await;
		let worker = Worker::new(
			graph.clone(),
			LlmClient::new_embed_only(&url, "m", ""),
			None,
			None,
			None,
		);

		// Every text embeds to the same stub vector, so the default threshold would
		// merge the three entrances into one entity and hide two of them.
		let no_dedup = Config {
			dedup_threshold: 2.0,
			..Config::default()
		};
		let file = || Source::File {
			path: "p".into(),
			section: String::new(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		};

		worker
			.run(
				"entered through run".into(),
				file(),
				EntityKind::Document,
				String::new(),
				1.0,
				"file",
				no_dedup.clone(),
				Scoping::default(),
			)
			.await;
		worker
			.submit(
				"entered through submit".into(),
				file(),
				EntityKind::Document,
				String::new(),
				1.0,
				"file",
				no_dedup.clone(),
				None,
				Scoping::default(),
			)
			.await;
		worker.enqueue(
			"entered through enqueue".into(),
			file(),
			EntityKind::Document,
			String::new(),
			1.0,
			"file",
			no_dedup,
			Scoping::default(),
		);

		let want = base::base_constants::MAX_AI_CONFIDENCE;
		let mut seen: Vec<(String, f64)> = Vec::new();
		let cap = std::time::Instant::now() + std::time::Duration::from_secs(5);
		while std::time::Instant::now() < cap {
			seen = graph
				.read()
				.kerns
				.values()
				.flat_map(|k| {
					k.entities
						.values()
						.map(|e| (e.statements.join(" "), e.conf_mean()))
				})
				.collect();
			if seen.len() >= 3 {
				break;
			}
			tokio::time::sleep(std::time::Duration::from_millis(25)).await;
		}

		assert!(
			seen.len() >= 3,
			"all three entrances reached the graph, got {seen:?}"
		);
		for entrance in ["run", "submit", "enqueue"] {
			let hit = seen
				.iter()
				.find(|(text, _)| text.contains(entrance))
				.unwrap_or_else(|| panic!("{entrance} placed nothing; got {seen:?}"));
			assert!(
				(hit.1 - (1.0 + want) / 3.0).abs() < 1e-6,
				"{entrance} minted an unclamped confidence: posterior {:.4}, want {:.4}",
				hit.1,
				(1.0 + want) / 3.0
			);
		}
	}

	#[tokio::test]
	async fn run_assembles_a_failed_outcome_when_document_embedding_fails() {
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = dead_worker(graph.clone());
		let text = "a document that cannot be embedded".to_string();
		let outcome = worker
			.run(
				text.clone(),
				session_source(),
				EntityKind::Claim,
				String::new(),
				1.0,
				"session",
				Config::default(),
				Scoping::default(),
			)
			.await;

		assert_eq!(outcome.status, OutcomeStatus::Failed);
		assert_eq!(
			outcome.doc_id,
			util::content_hash(&text),
			"doc id is the content hash"
		);
		assert!(
			outcome.total_chunks >= 1,
			"non-empty text splits into at least one chunk"
		);
		assert_eq!(
			outcome.failed_chunks, outcome.total_chunks,
			"all chunks counted as failed"
		);
		assert_eq!(outcome.embedded_chunks, 0);
		assert_eq!(
			outcome.failures.len(),
			1,
			"one document-level failure recorded"
		);
		assert_eq!(
			outcome.transient_failures + outcome.permanent_failures,
			1,
			"the single failure is classified exactly once",
		);
		assert_eq!(outcome.message, "document embedding failed");

		let g = graph.read();
		assert_eq!(g.all().iter().map(|k| k.entities.len()).sum::<usize>(), 0);
	}
}

#[cfg(test)]
mod outcome_tests {
	use super::*;

	#[test]
	fn status_as_str_is_the_lowercase_label() {
		assert_eq!(OutcomeStatus::Committed.as_str(), "committed");
		assert_eq!(OutcomeStatus::Partial.as_str(), "partial");
		assert_eq!(OutcomeStatus::Failed.as_str(), "failed");
		assert_eq!(OutcomeStatus::Deduped.as_str(), "deduped");
	}

	#[test]
	fn failed_zeroes_every_counter_and_carries_the_failures() {
		let f = FailureReport::document_permanent("boom");
		let o = Outcome::failed("nothing processed", vec![f]);
		assert_eq!(o.status, OutcomeStatus::Failed);
		assert!(o.doc_id.is_empty(), "no document committed");
		assert_eq!(
			(
				o.total_chunks,
				o.embedded_chunks,
				o.failed_chunks,
				o.transient_failures,
				o.permanent_failures
			),
			(0, 0, 0, 0, 0),
			"all counters zero on a wholly-failed outcome"
		);
		assert_eq!(o.failures.len(), 1, "failure detail preserved");
		assert_eq!(o.message, "nothing processed");
	}

	#[test]
	fn document_permanent_has_the_canonical_shape() {
		let f = FailureReport::document_permanent("lock poisoned");
		assert_eq!(f.scope, "document");
		assert_eq!(f.chunk_index, 0);
		assert_eq!(f.class, "permanent");
		assert_eq!(f.error, "lock poisoned");
	}
}

#[cfg(test)]
mod embed_tests {
	use super::*;
	use serde_json::{json, Value};

	fn echo_count_app() -> axum::Router {
		axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|body: axum::Json<Value>| async move {
				let n = body
					.0
					.get("input")
					.and_then(|v| v.as_array())
					.map(|a| a.len())
					.unwrap_or(1);
				let embs: Vec<Vec<f32>> = (0..n).map(|_| vec![0.1, 0.2, 0.3]).collect();
				axum::Json(json!({ "embeddings": embs }))
			}),
		)
	}

	#[tokio::test]
	async fn embed_chunks_short_circuits_on_the_batch_path_when_counts_match() {
		let (url, _server) = crate::test_support::spawn_http(echo_count_app()).await;
		let client = LlmClient::new_embed_only(&url, "m", "");
		let (vecs, fails) = embed_chunks(&client, &["a".into(), "b".into()]).await;
		assert_eq!(vecs.len(), 2);
		assert!(
			vecs.iter().all(|v| !v.is_empty()),
			"both embedded via the batch call"
		);
		assert!(fails.is_empty());
	}

	#[tokio::test]
	async fn embed_chunks_falls_back_to_per_item_on_a_count_mismatch() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|| async { axum::Json(json!({ "embeddings": [[0.5, 0.6]] })) }),
		);
		let (url, _server) = crate::test_support::spawn_http(app).await;
		let client = LlmClient::new_embed_only(&url, "m", "");
		let (vecs, fails) = embed_chunks(&client, &["a".into(), "b".into()]).await;
		assert_eq!(vecs.len(), 2);
		assert!(
			vecs.iter().all(|v| !v.is_empty()),
			"per-item fallback embedded each chunk"
		);
		assert!(fails.is_empty());
	}

	#[tokio::test]
	async fn embed_with_retry_treats_an_empty_response_as_permanent() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|| async { axum::Json(json!({ "embeddings": [] })) }),
		);
		let (url, _server) = crate::test_support::spawn_http(app).await;
		let client = LlmClient::new_embed_only(&url, "m", "");
		let fail = embed_with_retry(&client, "x", "chunk", 0)
			.await
			.unwrap_err();
		assert_eq!(fail.class, "permanent");
		assert_eq!(fail.scope, "chunk");
	}

	#[tokio::test]
	async fn embed_with_retry_treats_a_connection_failure_as_transient() {
		let client = LlmClient::new_embed_only("http://127.0.0.1:1", "m", "");
		let fail = embed_with_retry(&client, "x", "document", 3)
			.await
			.unwrap_err();
		assert_eq!(fail.class, "transient");
		assert_eq!(fail.chunk_index, 3);
	}

	#[tokio::test]
	async fn embed_chunks_empty_input_short_circuits_to_empty() {
		let (url, _server) = crate::test_support::spawn_http(echo_count_app()).await;
		let client = LlmClient::new_embed_only(&url, "m", "");
		let (vecs, fails) = embed_chunks(&client, &[]).await;
		assert!(vecs.is_empty() && fails.is_empty());
	}
}

#[cfg(test)]
mod split_tests {
	use super::*;

	#[test]
	fn paragraph_split_single_and_multi() {
		assert_eq!(
			paragraph_split("one paragraph"),
			vec!["one paragraph".to_string()]
		);
		assert_eq!(
			paragraph_split("first\n\nsecond"),
			vec!["first".to_string(), "second".to_string()]
		);
		assert_eq!(
			paragraph_split("  a  \n\n\n\n  b  "),
			vec!["a".to_string(), "b".to_string()]
		);
	}

	#[test]
	fn whitespace_or_empty_input_yields_no_chunks() {
		assert!(paragraph_split("").is_empty(), "empty -> no chunks");
		assert!(
			paragraph_split("   \n\n \t ").is_empty(),
			"whitespace-only -> no bogus chunk"
		);
	}

	#[test]
	fn split_uses_llm_statements_when_present() {
		let llm = |_p: &str| "stmt one\nstmt two".to_string();
		assert_eq!(
			split("raw", "", Some(&llm)),
			vec!["stmt one".to_string(), "stmt two".to_string()]
		);
	}

	#[test]
	fn split_falls_back_to_paragraphs_when_llm_returns_empty() {
		let llm = |_p: &str| String::new();
		assert_eq!(
			split("para a\n\npara b", "", Some(&llm)),
			vec!["para a".to_string(), "para b".to_string()],
			"empty LLM response -> paragraph fallback"
		);
	}

	#[test]
	fn split_without_llm_uses_paragraph_split() {
		assert_eq!(
			split("x\n\ny", "", None),
			vec!["x".to_string(), "y".to_string()]
		);
	}

	#[test]
	fn llm_split_folds_hint_into_the_prompt() {
		let seen = std::cell::RefCell::new(String::new());
		let llm = |p: &str| {
			*seen.borrow_mut() = p.to_string();
			"s".to_string()
		};
		let _ = llm_split("body", "rust code", &llm);
		assert!(
			seen.borrow().contains("describes rust code"),
			"hint is named in the prompt"
		);
		let _ = llm_split("body", "", &llm);
		assert!(!seen.borrow().contains("describes"));
	}
}
