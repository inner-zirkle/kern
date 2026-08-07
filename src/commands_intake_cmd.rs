//! The `intake` subcommand: drain and inspect the durable drop-dir queue.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use parking_lot::RwLock;

use ingest::intake_status::{scan, Report};
use store::base_store::FlushOutcome;

use crate::commands::{load_graph, Client, Endpoint, IntakeAction};
use crate::commands_route::{route, u64_field, Routed};

const WRITE_RETRIES: u32 = 5;

pub(crate) async fn cmd_intake(
	cfg: &config::Config,
	action: Option<IntakeAction>,
	embed_url: &str,
	embed_model: &str,
	reason_url: &str,
	reason_model: &str,
) {
	let dir = std::env::current_dir()
		.unwrap_or_else(|_| std::path::PathBuf::from("."))
		.join(&cfg.intake.dir);

	match action {
		Some(IntakeAction::Drain) => {
			drain(cfg, &dir, embed_url, embed_model, reason_url, reason_model).await
		}
		None | Some(IntakeAction::Status) => print_report(
			&cfg.intake.dir,
			&scan(&dir, SystemTime::now()),
			cfg.intake.enabled,
		),
	}
}

fn print_report(dir: &str, r: &Report, enabled: bool) {
	if !r.dir_exists {
		println!("intake {dir}: no queue directory yet");
		return;
	}
	println!(
		"intake {dir}  pending={} stuck={} failed={} done={}{}",
		r.pending.len(),
		r.stuck(),
		r.failed.len(),
		r.done,
		if enabled {
			""
		} else {
			"  [intake disabled in config]"
		}
	);

	for p in &r.pending {
		let age = p.age.map(human_age).unwrap_or_else(|| "?".into());
		match &p.last_error {
			// A stuck delta retries forever rather than losing the capture, so
			// the error is the only thing distinguishing it from one that simply
			// has not been picked up yet.
			Some(e) => println!("  STUCK  {:<28} {:>6}  {e}", p.name, age),
			None => println!("  wait   {:<28} {:>6}", p.name, age),
		}
	}
	for f in &r.failed {
		println!("  failed {f}  (not text; quarantined, never retried)");
	}
}

fn human_age(d: Duration) -> String {
	let s = d.as_secs();
	match s {
		0..=59 => format!("{s}s"),
		60..=3599 => format!("{}m", s / 60),
		3600..=86399 => format!("{}h", s / 3600),
		_ => format!("{}d", s / 86400),
	}
}

async fn drain(
	cfg: &config::Config,
	dir: &std::path::Path,
	embed_url: &str,
	embed_model: &str,
	reason_url: &str,
	reason_model: &str,
) {
	let before = scan(dir, SystemTime::now());
	if !before.dir_exists {
		println!("intake {}: no queue directory yet", cfg.intake.dir);
		return;
	}
	if before.pending.is_empty() {
		println!("intake {}: nothing pending", cfg.intake.dir);
		return;
	}

	let archived = match route("intake_drain", serde_json::json!({})).await {
		Routed::Done(v) => u64_field(&v, "archived") as usize,
		Routed::Refused(e) => return eprintln!("{e}"),
		Routed::NoDaemon => {
			drain_locally(
				cfg,
				dir,
				&before,
				embed_url,
				embed_model,
				reason_url,
				reason_model,
			)
			.await
		}
	};

	let after = scan(dir, SystemTime::now());
	println!(
		"drained {archived} of {} pending; {} still queued ({} stuck)",
		before.pending.len(),
		after.pending.len(),
		after.stuck()
	);
	print_report(&cfg.intake.dir, &after, cfg.intake.enabled);
}

// The `NoDaemon` half: nothing is serving, so this process owns the queue and
// the graph both. The queue itself is on disk either way, which is why only the
// archived count crosses the socket and both paths print through the same tail.
async fn drain_locally(
	cfg: &config::Config,
	dir: &std::path::Path,
	before: &Report,
	embed_url: &str,
	embed_model: &str,
	reason_url: &str,
	reason_model: &str,
) -> usize {
	let g = Arc::new(RwLock::new(load_graph(cfg)));
	let llm_client = Client::new(
		Endpoint::new(reason_url, reason_model, cfg.reason_key()),
		Endpoint::new(embed_url, embed_model, &cfg.embed.key),
	)
	.with_timeout_secs(cfg.reason.timeout_secs);
	let llm_fn: Option<ingest::LlmFunc> = if reason_url.is_empty() {
		// Only worth saying when something in the queue actually needs it —
		// documents drain fine with no reason model at all.
		if before.pending.iter().any(|p| p.name.ends_with(".txt")) {
			println!("no [reason] endpoint configured — transcripts (.txt) cannot be distilled and will be left queued");
		}
		None
	} else {
		let c = llm_client.clone();
		Some(Arc::new(move |q: &str| {
			llm::block_on_in_place(c.complete(q))
				.and_then(Result::ok)
				.unwrap_or_default()
		}))
	};
	let extra_kinds: Vec<String> = g.read().root.claim_kinds.keys().cloned().collect();
	let worker = ingest::Worker::new(g.clone(), llm_client, None, None, None);

	let archived = ingest::intake::drain_now(
		dir,
		&worker,
		llm_fn.as_ref(),
		&extra_kinds,
		cfg.ingest.dedup_threshold,
		cfg.intake.retention_secs,
		cfg.ingest.review_policy.clone(),
		Duration::from_secs(cfg.intake.done_retention_secs),
		SystemTime::now(),
	)
	.await;

	flush(&g, cfg);
	archived
}

// Same guarded retry as `cmd_ingest`: this opens the store directly, so a
// running daemon is a second writer (ROADMAP item 9). The guard turns that
// into a refused flush and a reload, never a silent clobber.
fn flush(g: &Arc<RwLock<graph::graph::GraphGnn>>, cfg: &config::Config) {
	for attempt in 0..WRITE_RETRIES {
		let expected = g.read().flushed_epoch();
		let flushed = graph::persist::flush_guarded(&g.read(), expected);
		match flushed {
			Ok(FlushOutcome::Flushed { .. }) => return,
			Ok(FlushOutcome::RefusedStale { .. }) if attempt + 1 < WRITE_RETRIES => {
				let mut w = g.write();
				let fresh = crate::commands::reload_graph(cfg, &w);
				*w = fresh;
			}
			Ok(FlushOutcome::RefusedStale {
				disk_epoch,
				expected,
			}) => {
				eprintln!(
					"intake drain: persisted under contention after {WRITE_RETRIES} tries \
					 (disk epoch {disk_epoch} vs {expected}); another writer is active on this data_dir"
				);
				return;
			}
			Err(e) => {
				eprintln!("save: {e}");
				return;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use ingest::intake_status::Pending;

	#[test]
	fn ages_read_in_the_largest_unit_that_fits() {
		assert_eq!(human_age(Duration::from_secs(0)), "0s");
		assert_eq!(human_age(Duration::from_secs(59)), "59s");
		assert_eq!(human_age(Duration::from_secs(60)), "1m");
		assert_eq!(human_age(Duration::from_secs(3599)), "59m");
		assert_eq!(human_age(Duration::from_secs(3600)), "1h");
		assert_eq!(human_age(Duration::from_secs(86_400)), "1d");
	}

	#[test]
	fn a_stuck_delta_is_distinguishable_from_one_merely_waiting() {
		let r = Report {
			dir_exists: true,
			pending: vec![
				Pending {
					name: "fresh.txt".into(),
					age: Some(Duration::from_secs(5)),
					last_error: None,
				},
				Pending {
					name: "stuck.txt".into(),
					age: Some(Duration::from_secs(7200)),
					last_error: Some("status=failed embed/transient: refused".into()),
				},
			],
			failed: vec!["blob.bin".into()],
			done: 3,
		};
		assert_eq!(r.stuck(), 1, "only the one carrying an error is stuck");
	}
}
