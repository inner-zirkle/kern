//! The durable drop path: write a payload into the intake dir as a file, so
//! ingest survives a crash between accept and commit — the queue is the disk.

use std::path::Path;

use crate::ingest::Worker;
use crate::ingest_worker::OutcomeStatus;
use base::base_constants::AGENT_SOURCE;
use base::base_types::{EntityKind, Scoping, Source};

use serde::{Deserialize, Serialize};

// Serialized as serde_json (name-based) — the bincode positional law does not apply here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectJob {
	pub text: String,
	pub source: Source,
	pub kind: EntityKind,
	pub hint: String,
	pub confidence: f64,
	// Absolute, not a duration: the deadline was fixed when the caller asked,
	// and this payload may sit in the intake for a whole poll interval first.
	#[serde(default)]
	pub valid_until: Option<std::time::SystemTime>,
	// Lower bi-temporal bound: when the claim became true. Carried through the
	// durable intake so the drain preserves the distiller's per-claim valid_from.
	#[serde(default)]
	pub valid_from: Option<std::time::SystemTime>,
	// The channel this payload arrived on — what `clamp_confidence` reads and
	// what `RetrievalConfig::source_trust` weights on. Carried rather than
	// re-derived at the drain: every payload here used to be minted by the MCP
	// tool, and a drain that renamed the principal would relabel a watched file
	// as an agent assertion. Payloads written before this field existed were
	// exactly that MCP mint, so the serde default is the agent it named inline.
	#[serde(default = "default_source_tag")]
	pub source_tag: String,
	#[serde(default)]
	pub scoping: Scoping,
}

fn default_source_tag() -> String {
	AGENT_SOURCE.to_string()
}

pub fn intake_direct(direct_dir: &Path, job: &DirectJob) -> std::io::Result<String> {
	std::fs::create_dir_all(direct_dir)?;
	let doc_id = util::content_hash(&job.text);
	let dst = direct_dir.join(format!("{doc_id}.json"));
	let tmp = direct_dir.join(format!("{doc_id}.{}.tmp", std::process::id()));
	let payload = serde_json::to_vec(job).map_err(std::io::Error::other)?;
	std::fs::write(&tmp, payload)?;
	if let Err(e) = std::fs::rename(&tmp, &dst) {
		let _ = std::fs::remove_file(&tmp);
		// dst already existing (concurrent identical intake) is success.
		if !dst.exists() {
			return Err(e);
		}
	}
	Ok(doc_id)
}

pub async fn drain_direct_once(
	direct_dir: &Path,
	worker: &Worker,
	cfg: &crate::ingest::Config,
) -> usize {
	let entries = match std::fs::read_dir(direct_dir) {
		Ok(e) => e,
		Err(_) => return 0,
	};
	let done = direct_dir.join("done");
	let mut archived = 0;
	for ent in entries.flatten() {
		let path = ent.path();
		if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("json") {
			continue;
		}
		let job: DirectJob = match std::fs::read_to_string(&path)
			.map_err(|e| e.to_string())
			.and_then(|raw| serde_json::from_str(&raw).map_err(|e| e.to_string()))
		{
			Ok(j) => j,
			Err(e) => {
				tracing::warn!(
					target: "kern.ingest.direct",
					path = %path.display(),
					error = %e,
					"unreadable direct payload; archiving as poison (retry cannot succeed)"
				);
				crate::ingest_intake::archive(&path, &done);
				archived += 1;
				continue;
			}
		};
		let job_cfg = crate::ingest::Config {
			valid_until: job.valid_until,
			valid_from: job.valid_from,
			..cfg.clone()
		};
		let outcome = worker
			.run(
				job.text,
				job.source,
				job.kind,
				job.hint,
				job.confidence,
				// The producer's own tag, not this drain's: the durable hop is a
				// carrier, and a drain that renamed it would relabel every channel
				// that ever routes through the intake as an agent assertion.
				&job.source_tag,
				job_cfg,
				job.scoping,
			)
			.await;
		if matches!(outcome.status, OutcomeStatus::Failed) {
			tracing::warn!(
				target: "kern.ingest.direct",
				path = %path.display(),
				status = outcome.status.as_str(),
				"direct ingest failed; leaving payload for retry"
			);
			continue;
		}
		crate::ingest_intake::archive(&path, &done);
		archived += 1;
	}
	archived
}

#[cfg(test)]
#[path = "tests/ingest_direct_test.rs"]
mod ingest_direct_tests;
