//! `kern export` / `kern import`: the whole hot graph as versioned JSON — the
//! backup that survives a FORMAT_VERSION wipe. The persisted LMDB format is
//! deliberately wiped, never migrated (alpha policy); this file is the stable
//! domain-level escape hatch. Import is a CRDT union with the same semantics
//! as `kern hub merge`, so re-importing an export is idempotent.

use std::collections::HashMap;
use std::time::SystemTime;

use base::base_types::Kern;
use serde::{Deserialize, Serialize};

use crate::{fail, hint, load_graph};

const EXPORT_FORMAT: &str = "kern-export";
const EXPORT_VERSION: u32 = 1;

/// The bi-temporal clocks ride beside the entities: they are `serde(skip)` on
/// `Entity` (the store persists them in a side map, not the entity bytes), so
/// a plain `Kern` serialization would silently drop them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Bitemporal {
	valid_from: Option<SystemTime>,
	valid_to: Option<SystemTime>,
	invalidated_at: Option<SystemTime>,
}

#[derive(Serialize, Deserialize)]
struct Export {
	format: String,
	version: u32,
	// The stamp the vectors were embedded under. Import refuses a mismatch:
	// vectors from another model score as noise against this store's, and
	// cosine truncates rather than failing, so nothing else would ever notice.
	embed_model: String,
	embed_dim: usize,
	exported_at: Option<SystemTime>,
	kerns: Vec<Kern>,
	// entity_id → clocks. Ids are content hashes, so the map is global.
	bitemporal: HashMap<String, Bitemporal>,
}

fn build_export(g: &mut graph::graph::GraphGnn) -> Export {
	let stamp = g.store().and_then(|s| s.embed_stamp()).unwrap_or_default();
	let ids = g.all_ids();
	let mut kerns: Vec<Kern> = Vec::with_capacity(ids.len());
	let mut bitemporal: HashMap<String, Bitemporal> = HashMap::new();
	// Through `get`, not `all`: `get` loads an unloaded kern on demand, so the
	// export covers the whole persisted graph, not just the resident set.
	for id in &ids {
		let Some(kern) = g.get(id) else {
			continue;
		};
		for t in kern.entities.values() {
			if t.valid_from.is_some() || t.valid_to.is_some() || t.invalidated_at.is_some() {
				bitemporal.insert(
					t.id.clone(),
					Bitemporal {
						valid_from: t.valid_from,
						valid_to: t.valid_to,
						invalidated_at: t.invalidated_at,
					},
				);
			}
		}
		kerns.push(kern.clone());
	}
	Export {
		format: EXPORT_FORMAT.into(),
		version: EXPORT_VERSION,
		embed_model: stamp.model,
		embed_dim: stamp.dim,
		exported_at: Some(SystemTime::now()),
		kerns,
		bitemporal,
	}
}

/// The import core: CRDT union plus the bi-temporal re-stamp. Returns rows
/// joined. Local clocks win — a clock already set is a local observation.
fn apply_import(g: &mut graph::graph::GraphGnn, export: Export) -> usize {
	let mut disk = graph::graph::GraphGnn::new();
	disk.kerns = export
		.kerns
		.into_iter()
		.map(|k| (k.id.clone(), k))
		.collect();
	let changed = graph::merge::absorb_graph(g, disk);
	for (id, clocks) in &export.bitemporal {
		let Some(kid) = g.kern_of_entity(id).map(str::to_string) else {
			continue;
		};
		let Some(t) = g.kerns.get_mut(&kid).and_then(|k| k.entities.get_mut(id)) else {
			continue;
		};
		t.valid_from = t.valid_from.or(clocks.valid_from);
		t.valid_to = t.valid_to.or(clocks.valid_to);
		t.invalidated_at = t.invalidated_at.or(clocks.invalidated_at);
	}
	changed
}

pub(crate) fn cmd_export(cfg: &config::Config, out: &str) {
	let mut g = load_graph(cfg);
	let export = build_export(&mut g);
	let entities: usize = export.kerns.iter().map(|k| k.entities.len()).sum();
	let payload = match serde_json::to_vec(&export) {
		Ok(p) => p,
		Err(e) => return fail("export", e),
	};
	if let Err(e) = std::fs::write(out, payload) {
		return fail("export", format!("writing {out} failed: {e}"));
	}
	println!(
		"exported {} kern(s), {} thought(s) to {out} (hot graph; the cold tier spills back on its own use)",
		export.kerns.len(),
		entities
	);
}

pub(crate) fn cmd_import(cfg: &config::Config, file: &str, force: bool) {
	if let Some(who) = store::lock::holder(&cfg.data_dir) {
		fail(
			"import",
			format!("refused — the writer lock is held by {who}"),
		);
		return hint("a daemon serving this directory? stop it first");
	}
	let raw = match std::fs::read_to_string(file) {
		Ok(r) => r,
		Err(e) => return fail("import", format!("reading {file} failed: {e}")),
	};
	let export: Export = match serde_json::from_str(&raw) {
		Ok(x) => x,
		Err(e) => return fail("import", format!("{file} is not a kern export: {e}")),
	};
	if export.format != EXPORT_FORMAT || export.version != EXPORT_VERSION {
		// Alpha policy for the export format too: refuse loudly, never sniff.
		return fail(
			"import",
			format!(
				"{} v{} is not {EXPORT_FORMAT} v{EXPORT_VERSION}",
				export.format, export.version
			),
		);
	}

	let mut g = load_graph(cfg);
	let stamp = g.store().and_then(|s| s.embed_stamp()).unwrap_or_default();
	if !force
		&& !stamp.model.is_empty()
		&& !export.embed_model.is_empty()
		&& (stamp.model != export.embed_model || stamp.dim != export.embed_dim)
	{
		fail(
			"import",
			format!(
				"refused — the export was embedded under {}({}d) and this store under {}({}d); the imported vectors would score as noise",
				export.embed_model, export.embed_dim, stamp.model, stamp.dim
			),
		);
		return hint("re-export from a re-embedded store, or --force and then `kern reembed`");
	}

	let before = ::health::graph_health_stats(&g);
	let changed = apply_import(&mut g, export);
	if let Err(e) = graph::persist::save_all(&g) {
		fail("import", format!("save failed: {e}"));
	}
	g.consolidate_disk_index();
	let after = ::health::graph_health_stats(&g);
	println!(
		"imported {file}: {} row(s) joined, entities {} -> {}, kerns {} -> {}",
		changed, before.entities, after.entities, before.kerns, after.kerns
	);
}

#[cfg(test)]
#[path = "tests/commands_export_test.rs"]
mod commands_export_tests;
