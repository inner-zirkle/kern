//! The durable drop path: write a payload into the intake dir as a file, so
//! ingest survives a crash between accept and commit — the queue is the disk.

use std::path::Path;

use crate::base_constants::AGENT_SOURCE;
use crate::base_types::{EntityKind, Scoping, Source};
use crate::ingest::Worker;
use crate::ingest_outcome::OutcomeStatus;
use crate::util;

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
mod tests {
	use super::*;
	use crate::graph::GraphGnn;
	use parking_lot::RwLock;
	use std::sync::Arc;
	use std::time::Duration;
	use tempfile::tempdir;

	fn job(text: &str) -> DirectJob {
		DirectJob {
			text: text.to_string(),
			source: Source::Inline {
				hash: "obj-1".into(),
				section: String::new(),
			},
			kind: EntityKind::Claim,
			hint: "audit-finding".into(),
			confidence: 0.7,
			valid_until: None,
			valid_from: None,
			source_tag: AGENT_SOURCE.to_string(),
			scoping: Scoping::default(),
		}
	}

	#[test]
	fn intake_direct_writes_idempotent_json_named_by_content_hash() {
		let dir = tempdir().unwrap();
		let direct = dir.path().join("direct");

		let id1 = intake_direct(&direct, &job("a durable fact")).expect("accepted");
		let id2 = intake_direct(&direct, &job("a durable fact")).expect("re-submit ok");
		assert_eq!(
			id1,
			util::content_hash("a durable fact"),
			"doc id is the content hash"
		);
		assert_eq!(id1, id2, "same text -> same file, idempotent");

		let files: Vec<_> = std::fs::read_dir(&direct)
			.unwrap()
			.flatten()
			.filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
			.collect();
		assert_eq!(files.len(), 1, "one file per unique payload");

		let raw = std::fs::read_to_string(files[0].path()).unwrap();
		let back: DirectJob = serde_json::from_str(&raw).expect("valid json payload");
		assert_eq!(back.text, "a durable fact");
		assert_eq!(back.confidence, 0.7);
	}

	#[test]
	fn valid_from_round_trips_through_json() {
		let now = std::time::SystemTime::now();
		let j = DirectJob {
			text: "round-trip".into(),
			source: Source::Inline {
				hash: "h".into(),
				section: String::new(),
			},
			kind: EntityKind::Claim,
			hint: String::new(),
			confidence: 0.5,
			valid_until: Some(now),
			valid_from: Some(now),
			source_tag: AGENT_SOURCE.to_string(),
			scoping: Scoping::default(),
		};
		let json = serde_json::to_string(&j).unwrap();
		let back: DirectJob = serde_json::from_str(&json).unwrap();
		assert_eq!(back.valid_from, Some(now));
		assert_eq!(back.valid_until, Some(now));
	}

	#[test]
	fn old_payload_without_valid_from_deserializes_as_none() {
		// Simulate a payload written before valid_from existed.
		let json = r#"{"text":"old","source":{"Inline":{"hash":"h","section":""}},"kind":"Claim","hint":"","confidence":0.7,"valid_until":null,"source_tag":"agent"}"#;
		let j: DirectJob = serde_json::from_str(json).unwrap();
		assert_eq!(j.valid_from, None, "missing field defaults to None");
	}

	#[tokio::test]
	async fn drain_direct_once_ingests_and_archives_end_to_end() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|_b: axum::Json<serde_json::Value>| async move {
				axum::Json(serde_json::json!({ "embeddings": [[0.1, 0.2, 0.3]] }))
			}),
		);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

		let embedder = crate::llm::Client::new_embed_only(&format!("http://{addr}"), "m", "");
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = Worker::new(graph.clone(), embedder, None, None, None);

		let dir = tempdir().unwrap();
		let direct = dir.path().join("direct");
		let deadline = std::time::SystemTime::now() + Duration::from_secs(3600);
		let mut j = job("the spawn gate shipped today");
		j.valid_until = Some(deadline);
		let doc_id = intake_direct(&direct, &j).expect("accepted");

		let cfg = crate::ingest::Config {
			dedup_threshold: 0.95,
			..Default::default()
		};
		let archived = drain_direct_once(&direct, &worker, &cfg).await;

		assert_eq!(archived, 1, "the job committed -> archived");
		assert!(
			direct.join("done").join(format!("{doc_id}.json")).exists(),
			"intake file moved into direct/done/"
		);
		let g = graph.read();
		let total: usize = g.all().iter().map(|k| k.entities.len()).sum();
		assert!(
			total > 0,
			"the payload flowed through the worker into the graph"
		);
		assert!(
			g.all()
				.iter()
				.flat_map(|k| k.entities.values())
				.all(|e| e.valid_until == Some(deadline)),
			"the retention survives the durable intake round-trip"
		);

		server.abort();
	}

	// The tag is the channel (ROADMAP item 95), and this hop is where it could be
	// lost: the drain used to name AGENT_SOURCE for every payload, because every
	// payload was an MCP mint. A relabel is numerically invisible for any tag but
	// one — `clamp_confidence` only separates USER_SOURCE — so the guard is a
	// user-tagged payload, whose 1.0 survives only if its own tag reached the clamp.
	#[tokio::test]
	async fn drain_direct_once_clamps_against_the_payloads_tag_not_a_fixed_principal() {
		let (url, _server) =
			crate::test_support::spawn_http(crate::test_support::fixed_vec_embed_app()).await;
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let embedder = crate::llm::Client::new_embed_only(&url, "m", "");
		let worker = Worker::new(graph.clone(), embedder, None, None, None);

		let dir = tempdir().unwrap();
		let direct = dir.path().join("direct");
		let mut j = job("a human said so");
		j.confidence = 1.0;
		j.source_tag = crate::base_constants::USER_SOURCE.to_string();
		intake_direct(&direct, &j).expect("accepted");

		let cfg = crate::ingest::Config {
			dedup_threshold: 0.95,
			..Default::default()
		};
		assert_eq!(
			drain_direct_once(&direct, &worker, &cfg).await,
			1,
			"the job committed"
		);

		// `conf_beta`, not `conf_mean`: only alpha accrues evidence after the mint,
		// so beta is the field that still reports what was MINTED —
		// beta_params_from_confidence(c) gives beta = 2 - c.
		let betas: Vec<f32> = graph
			.read()
			.kerns
			.values()
			.flat_map(|k| k.entities.values().map(|e| e.conf_beta))
			.collect();
		assert!(!betas.is_empty(), "the payload reached the graph");
		for got in &betas {
			assert!(
				(got - 1.0).abs() < 1e-6,
				"the payload's own tag reached the clamp: conf_beta want 1.0000, got {got:.4} \
				 (1.0500 is the drain renaming it to agent)"
			);
		}
	}

	#[tokio::test]
	async fn drain_direct_once_leaves_failed_job_for_retry() {
		let embedder = crate::llm::Client::new_embed_only("http://127.0.0.1:1", "m", "");
		let graph = Arc::new(RwLock::new(GraphGnn::new()));
		let worker = Worker::new(graph, embedder, None, None, None);

		let dir = tempdir().unwrap();
		let direct = dir.path().join("direct");
		let doc_id = intake_direct(&direct, &job("must survive the outage")).expect("accepted");

		let cfg = crate::ingest::Config {
			dedup_threshold: 0.95,
			..Default::default()
		};
		let archived = tokio::time::timeout(
			Duration::from_secs(30),
			drain_direct_once(&direct, &worker, &cfg),
		)
		.await
		.expect("drain must not hang");

		assert_eq!(archived, 0, "failed job is not archived");
		assert!(
			direct.join(format!("{doc_id}.json")).exists(),
			"file left in the direct intake for retry"
		);
	}
}
