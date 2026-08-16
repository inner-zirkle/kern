//! `kern doctor` / `kern repair`: a strictly read-only, content-safe health
//! report, and a fail-closed repair that executes ONLY what a doctor manifest
//! authorizes. The split is the design (after mnemosyne's doctor/repair, MIT):
//! diagnosis never mutates, and repair has no discovery of its own — it cannot
//! decide to fix something the operator never saw in a report.

use serde::{Deserialize, Serialize};

use crate::{fail, hint, load_graph};

const MANIFEST_FORMAT: &str = "kern-doctor";
const MANIFEST_VERSION: u32 = 1;

/// The closed set of repairs `kern repair` may execute. Everything else a
/// finding can say ("re-embed", "check your config") is advice for a human —
/// no variant, no execution path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum RepairAction {
	/// Remove one reason whose endpoint entity no longer exists anywhere.
	DropDanglingReason { kern_id: String, reason_id: String },
	/// Reap kerns holding no entities (the residue of unnamed-kern churn).
	ReapEmptyKerns,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Finding {
	pub code: String,
	// "error" | "warn" | "info"
	pub severity: String,
	pub message: String,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub repairs: Vec<RepairAction>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Manifest {
	format: String,
	version: u32,
	pub findings: Vec<Finding>,
}

fn finding(code: &str, severity: &'static str, message: String) -> Finding {
	Finding {
		code: code.into(),
		severity: severity.into(),
		message,
		repairs: Vec::new(),
	}
}

pub(crate) fn diagnose(cfg: &config::Config, g: &graph::graph::GraphGnn) -> Manifest {
	let mut findings = Vec::new();

	if let Err(e) = cfg.validate() {
		findings.push(finding("config_invalid", "error", format!("config: {e}")));
	}
	if let Some(who) = store::lock::holder(&cfg.data_dir) {
		findings.push(finding(
			"writer_lock_held",
			"info",
			format!("writer lock held by {who} — this report reads a snapshot; repairs will refuse"),
		));
	}

	// The graph handed in was just loaded, so if any row needed the format hop,
	// `store_core` recorded it during that load. Reported as a finding rather
	// than left to a log line nobody greps.
	if let Some(from) = store_core::migrated_from() {
		findings.push(finding(
			"format_older_than_build",
			"warn",
			format!(
				"rows are stored in format v{from}; this build writes v{} and converts them on read. Recall works, but the store stays old until something writes — run `kern migrate` to finish it in one pass",
				store_core::format_version()
			),
		));
	}

	let h = ::health::graph_health_stats(g);
	if h.embed_mismatch {
		findings.push(finding(
			"embed_mismatch",
			"error",
			format!(
				"store was embedded under a different model than configured ({} @ {}d) — search silently misses; run `kern reembed`",
				h.embed_model, h.embed_dim
			),
		));
	}

	let data = std::path::Path::new(&cfg.data_dir).join("data.mdb");
	let len = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);
	if len > crate::SELF_HEAL_BLOAT_BYTES {
		findings.push(finding(
			"data_mdb_bloated",
			"warn",
			format!(
				"data.mdb is {} MiB — LMDB returns freed pages only on compaction; the next daemon boot self-heals, or run `kern gc` now",
				len / (1024 * 1024)
			),
		));
	}

	let mut vectorless = 0usize;
	let mut empty_kerns = 0usize;
	let mut dangling: Vec<RepairAction> = Vec::new();
	for kern in g.all() {
		if kern.entities.is_empty() && kern.graviton_text.is_empty() && kern.id != g.root.id {
			empty_kerns += 1;
		}
		for t in kern.entities.values() {
			if !t.has_vector() {
				vectorless += 1;
			}
		}
		for r in kern.reasons.values() {
			let from_ok = graph::search::find_entity(g, &r.from).is_some();
			let to_ok = graph::search::find_entity(g, &r.to).is_some();
			if !from_ok || !to_ok {
				dangling.push(RepairAction::DropDanglingReason {
					kern_id: kern.id.clone(),
					reason_id: r.id.clone(),
				});
			}
		}
	}
	if vectorless > 0 {
		findings.push(finding(
			"vectorless_entities",
			"warn",
			format!("{vectorless} thought(s) carry no embedding and are invisible to dense search — run `kern reembed`"),
		));
	}
	if !dangling.is_empty() {
		let mut f = finding(
			"dangling_reasons",
			"warn",
			format!(
				"{} reason edge(s) point at thoughts that no longer exist — dead weight every walk pays for",
				dangling.len()
			),
		);
		f.repairs = dangling;
		findings.push(f);
	}
	if empty_kerns > 0 {
		let mut f = finding(
			"empty_kerns",
			"info",
			format!("{empty_kerns} empty unnamed kern(s) — the residue of clustering churn"),
		);
		f.repairs = vec![RepairAction::ReapEmptyKerns];
		findings.push(f);
	}

	Manifest {
		format: MANIFEST_FORMAT.into(),
		version: MANIFEST_VERSION,
		findings,
	}
}

pub(crate) fn cmd_doctor(cfg: &config::Config, json: bool) {
	let g = load_graph(cfg);
	let manifest = diagnose(cfg, &g);
	if json {
		match serde_json::to_string_pretty(&manifest) {
			Ok(s) => println!("{s}"),
			Err(e) => fail("doctor", e),
		}
		return;
	}
	if manifest.findings.is_empty() {
		println!("doctor: no findings — store, config, and graph look healthy");
		return;
	}
	for f in &manifest.findings {
		println!("  [{}] {}: {}", f.severity, f.code, f.message);
	}
	let repairable = manifest
		.findings
		.iter()
		.filter(|f| !f.repairs.is_empty())
		.count();
	if repairable > 0 {
		println!(
			"{repairable} finding(s) carry repairs — `kern doctor --json > manifest.json`, review it, then `kern repair manifest.json`"
		);
	}
}

/// Executes ONLY the repairs the manifest carries. No discovery, no defaults:
/// an empty manifest repairs nothing, an unknown action fails the parse.
pub(crate) fn cmd_repair(cfg: &config::Config, file: &str) {
	if let Some(who) = store::lock::holder(&cfg.data_dir) {
		fail(
			"repair",
			format!("refused — the writer lock is held by {who}"),
		);
		return hint("a daemon serving this directory? stop it first");
	}
	let raw = match std::fs::read_to_string(file) {
		Ok(r) => r,
		Err(e) => return fail("repair", format!("reading {file} failed: {e}")),
	};
	let manifest: Manifest = match serde_json::from_str(&raw) {
		Ok(m) => m,
		Err(e) => return fail("repair", format!("{file} is not a doctor manifest: {e}")),
	};
	if manifest.format != MANIFEST_FORMAT || manifest.version != MANIFEST_VERSION {
		return fail(
			"repair",
			format!(
				"{} v{} is not {MANIFEST_FORMAT} v{MANIFEST_VERSION}",
				manifest.format, manifest.version
			),
		);
	}
	let mut g = load_graph(cfg);
	let (dropped, reaped) = apply_repairs(&mut g, &manifest);
	if dropped + reaped > 0 {
		if let Err(e) = graph::persist::save_all(&g) {
			fail("repair", format!("save failed: {e}"));
		}
		g.consolidate_disk_index();
	}
	println!("repair: dropped {dropped} dangling reason(s), reaped {reaped} empty kern(s)");
}

pub(crate) fn apply_repairs(g: &mut graph::graph::GraphGnn, manifest: &Manifest) -> (usize, usize) {
	let mut dropped = 0usize;
	let mut reaped = 0usize;
	for f in &manifest.findings {
		for r in &f.repairs {
			match r {
				RepairAction::DropDanglingReason { kern_id, reason_id } => {
					// Re-verify at execution time — fail closed: the graph may have
					// moved since the report, and a reason that grew a live endpoint
					// is no longer the reason the manifest described.
					let still_dangling = g
						.kerns
						.get(kern_id)
						.and_then(|k| k.reasons.get(reason_id))
						.is_some_and(|r| {
							let from_ok = graph::search::find_entity(g, &r.from).is_some();
							let to_ok = graph::search::find_entity(g, &r.to).is_some();
							!from_ok || !to_ok
						});
					if !still_dangling {
						continue;
					}
					if let Some(kern) = g.kerns.get_mut(kern_id) {
						graph::reason::remove_reason(kern, reason_id);
						dropped += 1;
					}
				}
				RepairAction::ReapEmptyKerns => {
					let (_, n, _) = g.gc_empty_kerns_counted();
					reaped += n;
				}
			}
		}
	}
	(dropped, reaped)
}

#[cfg(test)]
#[path = "tests/commands_doctor_test.rs"]
mod commands_doctor_tests;
