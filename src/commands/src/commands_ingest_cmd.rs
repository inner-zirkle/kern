//! The `ingest` subcommand: hand text or files to the ingest pipeline and
//! report the outcome.

use std::sync::Arc;

use parking_lot::RwLock;

use base::base_types::Source;
use math::clamp_confidence;
use store_core::FlushOutcome;
use util::truncate;

use crate::{load_graph, Client, Endpoint};

const WRITE_RETRIES: u32 = 5;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn cmd_ingest(
	cfg: &config::Config,
	text_parts: Vec<String>,
	file: Option<String>,
	retention_secs: u64,
	embed_url: &str,
	embed_model: &str,
	reason_url: &str,
	reason_model: &str,
) {
	let (embed_key, reason_key) = (&cfg.embed.key, cfg.reason_key());
	let text = if let Some(path) = file {
		// `main` re-pins cwd to the project root before the command runs, so a
		// relative `--file` given by the caller is resolved against the root and
		// not against the directory the caller was actually in. That failed with a
		// bare ENOENT in the good case and, when a same-named file happened to sit
		// at the root, silently ingested the WRONG file. Resolve against the
		// launch dir, which is what the caller meant.
		let resolved = crate::launch_dir_join(&path);
		match std::fs::read_to_string(&resolved) {
			Ok(t) => t,
			Err(e) => {
				eprintln!("read file {}: {e}", resolved.display());
				return;
			}
		}
	} else {
		text_parts.join(" ")
	};

	if text.is_empty() {
		eprintln!("text or --file required");
		return;
	}

	// Resolved once, before the retry loop: a retry must not push the deadline out.
	let valid_until = match ingest::valid_until_from_retention(retention_secs) {
		Ok(v) => v,
		Err(e) => {
			eprintln!("{e}");
			return;
		}
	};

	let g = Arc::new(RwLock::new(load_graph(cfg)));
	let llm_client = Client::new(
		Endpoint::new(reason_url, reason_model, reason_key),
		Endpoint::new(embed_url, embed_model, embed_key),
	)
	.with_timeout_secs(cfg.reason.timeout_secs)
	.with_num_ctx(cfg.reason.num_ctx)
	.with_reason_keep_alive(&cfg.reason.keep_alive)
	.with_embed_num_ctx(cfg.embed.num_ctx)
	.with_embed_keep_alive(&cfg.embed.keep_alive);
	let worker = ingest::Worker::new(g.clone(), llm_client, None, None, None);

	let (conf, kind) = clamp_confidence(1.0, "user");
	// Identity per ingest, not a shared constant: a constant hash made every
	// CLI ingest the same source, so each one superseded the previous fact.
	let src = Source::Inline {
		hash: util::content_hash(&text),
		section: String::new(),
	};

	let mut outcome = run_once(&worker, &g, &text, &src, kind, conf, cfg, valid_until).await;
	for attempt in 0..WRITE_RETRIES {
		// Guard against the epoch observed at LOAD time, not a re-read at flush time —
		// else a writer that committed in between gets overwritten unseen.
		let expected = g.read().flushed_epoch();
		// Bind before matching: a scrutinee temporary keeps the read guard alive
		// across the match — deadlocking the write() below.
		let flushed = graph::persist::flush_guarded(&g.read(), expected);
		match flushed {
			Ok(FlushOutcome::Flushed { .. }) => break,
			Ok(FlushOutcome::RefusedStale { .. }) if attempt + 1 < WRITE_RETRIES => {
				// Adopt the committed graph reusing the open store handle — never reopen the env.
				{
					let mut w = g.write();
					let fresh = crate::reload_graph(cfg, &w);
					*w = fresh;
				}
				outcome = run_once(&worker, &g, &text, &src, kind, conf, cfg, valid_until).await;
			}
			Ok(FlushOutcome::RefusedStale {
				disk_epoch,
				expected,
			}) => {
				eprintln!(
					"ingest: persisted under contention after {WRITE_RETRIES} tries \
					 (disk epoch {disk_epoch} vs {expected}); another writer is active on this data_dir"
				);
				break;
			}
			Err(e) => {
				eprintln!("save: {e}");
				break;
			}
		}
	}

	let summary = truncate(&text, 60);
	println!(
		"ingested {summary} (status={} chunks={})",
		outcome.status.as_str(),
		outcome.total_chunks
	);
	for f in &outcome.failures {
		eprintln!(
			"  {} #{} ({}): {}",
			f.scope, f.chunk_index, f.class, f.error
		);
	}
}

#[allow(clippy::too_many_arguments)]
async fn run_once(
	worker: &ingest::Worker,
	_g: &Arc<RwLock<graph::graph::GraphGnn>>,
	text: &str,
	src: &Source,
	kind: base::base_types::EntityKind,
	conf: f64,
	cfg: &config::Config,
	valid_until: Option<std::time::SystemTime>,
) -> ingest::Outcome {
	worker
		.run(
			text.to_string(),
			src.clone(),
			kind,
			String::new(),
			conf,
			// The CLI is the one path with a human behind it, and `Source::Inline`
			// cannot record that (ROADMAP item 20), so it names the principal here.
			base::base_constants::USER_SOURCE,
			ingest_config(cfg, valid_until),
			base::base_types::Scoping::default(),
		)
		.await
}

fn ingest_config(
	cfg: &config::Config,
	valid_until: Option<std::time::SystemTime>,
) -> ingest::Config {
	ingest::Config {
		dedup_threshold: cfg.ingest.dedup_threshold,
		dedup_threshold_by_kind: cfg.ingest.dedup_threshold_by_kind,
		valid_until,
		review_policy: cfg.ingest.review_policy.clone(),
		..Default::default()
	}
}

#[cfg(test)]
#[path = "tests/commands_ingest_cmd_test.rs"]
mod commands_ingest_cmd_tests;
