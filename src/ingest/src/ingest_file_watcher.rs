//! Bridge from the file watcher to ingest: watched-file events become ingest
//! jobs, a changed file supersedes the entities its previous revision produced
//! (including renames via the source index), and deletes tombstone them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use util::watcher::{
	FileWatcher, IgnoreRules, IngestPipeline, IngestRecord, IngestSink, WatcherError,
};

use crate::ingest::{Config as IngestRunConfig, Worker};
use base::base_types::{EntityKind, Scoping, Source};

fn strip_file_uri(uri: &str) -> String {
	if let Some(rest) = uri.strip_prefix("file:///") {
		// Windows `file:///C:/foo` → `C:/foo`; POSIX `file:///abs` → `abs`
		// (drops the empty authority's leading slash).
		return rest.to_string();
	}
	if let Some(rest) = uri.strip_prefix("file://") {
		// Non-empty authority (`file://host/path`): per RFC 8089 drop the host,
		// the local path is everything from the first '/'.
		return match rest.find('/') {
			Some(i) => rest[i..].to_string(),
			None => String::new(),
		};
	}
	uri.to_string()
}

#[derive(Clone)]
pub struct KernFileWatcherSink {
	worker: Arc<Worker>,
	retention_secs: u64,
	review_policy: crate::ingest::ReviewPolicy,
	hygiene: hygiene::GateConfig,
	// `<intake>/direct`, or `None` when nothing drains it. An undrained intake is
	// worse than the RAM queue — the same gate `tool_ingest` applies.
	direct_dir: Option<PathBuf>,
}

impl KernFileWatcherSink {
	pub fn new(
		worker: Arc<Worker>,
		retention_secs: u64,
		review_policy: crate::ingest::ReviewPolicy,
		hygiene: hygiene::GateConfig,
		direct_dir: Option<PathBuf>,
	) -> Self {
		Self {
			worker,
			retention_secs,
			review_policy,
			hygiene,
			direct_dir,
		}
	}

	// Per record, never once at construction: this sink lives as long as the
	// daemon, and a deadline resolved at startup would give a file edited a
	// month later a TTL measured from boot.
	fn ingest_config(&self) -> IngestRunConfig {
		IngestRunConfig {
			review_policy: self.review_policy.clone(),
			hygiene: self.hygiene.clone(),
			..Default::default()
		}
		.with_retention(self.retention_secs)
	}
}

#[async_trait]
impl IngestSink for KernFileWatcherSink {
	async fn ingest(&self, record: IngestRecord) {
		let IngestRecord {
			source_uri,
			content,
			language_hint,
			replaces,
		} = record;

		let path = strip_file_uri(&source_uri);
		let title = std::path::Path::new(&path)
			.file_name()
			.and_then(|s| s.to_str())
			.unwrap_or("")
			.to_string();

		let source = Source::File {
			path,
			section: String::new(),
			title,
			author: String::new(),
			url: source_uri,
		};

		let hint = language_hint.unwrap_or_default();

		// The channel, not a principal: nobody asserted this, a file changed on
		// disk. `scheme()` is also what `RetrievalConfig::source_trust` weights on,
		// so `source_trust = { file = ... }` is the lever that separates the
		// watcher from an agent — a `"watcher"` constant would only relabel the
		// same 0.95 ceiling.
		let tag = source.scheme();

		// Durable first, RAM second — `tool_ingest`'s shape. `notify` installs
		// watches and replays nothing, and there is no startup scan, so a record
		// still in the channel when the daemon dies is gone and nothing re-offers
		// it. The raw 1.0 travels rather than a pre-clamped value: `job()` is the
		// one clamp gate, and `source_tag` is what makes it clamp the same on both
		// legs. Fail open — a failed durable write falls through to the queue,
		// because a watcher that silently stops ingesting is the worse outcome.
		if let Some(dir) = &self.direct_dir {
			let cfg = self.ingest_config();
			let job = crate::ingest_direct::DirectJob {
				text: content.clone(),
				source: source.clone(),
				kind: EntityKind::Document,
				hint: hint.clone(),
				confidence: 1.0,
				valid_until: cfg.valid_until,
				valid_from: None,
				source_tag: tag.to_string(),
				scoping: Scoping::default(),
			};
			match crate::ingest_direct::intake_direct(dir, &job) {
				Ok(_) => return,
				Err(e) => tracing::warn!(
					target: "kern.ingest.direct",
					dir = %dir.display(),
					error = %e,
					"direct intake write failed; falling back to the in-RAM queue"
				),
			}
		}

		// `replaces` arrives as the old file URI; the graph keys on
		// `source_id()` (a content hash of scheme+object+section), so resolve it
		// here, once, rather than re-parsing inside the place path.
		let replaces_external = replaces.as_deref().map(|old_uri| {
			Source::File {
				path: strip_file_uri(old_uri),
				section: String::new(),
				title: String::new(),
				author: String::new(),
				url: old_uri.to_string(),
			}
			.source_id()
			.unwrap_or_default()
		});
		self
			.worker
			.submit(
				content,
				source,
				EntityKind::Document,
				hint,
				1.0,
				tag,
				self.ingest_config(),
				replaces_external,
				Scoping::default(),
			)
			.await;
	}
}

pub async fn run(
	roots: Vec<PathBuf>,
	ignore: IgnoreRules,
	sink: Arc<KernFileWatcherSink>,
) -> Result<(), WatcherError> {
	let mut watcher = FileWatcher::new(roots, ignore)?;
	let pipeline = IngestPipeline::new((*sink).clone());
	while let Some(ev) = watcher.next_event().await {
		pipeline.handle(ev).await;
	}
	Ok(())
}

#[cfg(test)]
#[path = "tests/ingest_file_watcher_test.rs"]
mod ingest_file_watcher_tests;
