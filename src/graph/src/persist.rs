//! Load and flush the graph against its LMDB store: [`load_dir`] opens a data
//! dir into a [`GraphGnn`], [`reload_from_disk`] re-reads through the already-
//! open env (a same-process double-open can SIGSEGV), and [`flush_guarded`]
//! writes back only when the store's flush epoch still matches the caller's
//! snapshot — the losing side of a write race is refused, not silently
//! overwritten.

use super::graph::GraphGnn;
use base::base_types::Kern;
use math::quant::QuantizationMode;
use std::collections::HashMap;

pub fn load_dir(dir: &str) -> Result<GraphGnn, store::base_store::StoreError> {
	use std::sync::Arc;
	use store::base_store::Store;

	let store = Arc::new(Store::open(dir)?);
	graph_from_store(store, dir)
}

// Reload MUST come through here, never load_dir: a same-process double-open of the
// LMDB env can SIGSEGV.
pub fn reload_from_disk(old: &GraphGnn) -> Option<GraphGnn> {
	let store = old.store()?;
	let dir = old.data_dir.clone();
	graph_from_store(store, &dir).ok()
}

fn graph_from_store(
	store: std::sync::Arc<store::base_store::Store>,
	dir: &str,
) -> Result<GraphGnn, store::base_store::StoreError> {
	let (kerns, mut network_id, quant_mode) = store.load_all_kerns()?;
	if network_id.is_empty() {
		network_id = util::uuid_v4();
	}

	let loaded_epoch = store.read_epoch();
	if !kerns.contains_key("root") {
		// Rows without a root is a bad read, not a fresh store. Returning an
		// empty graph stamped with the store's live epoch made it unfalsifiably
		// "current": reconcile saw nothing stale, and the first dirty flush
		// overwrote every row on disk with nothing. Error instead — the caller's
		// fallback boots at epoch 0, where the guarded flush refuses and absorbs.
		if !kerns.is_empty() {
			return Err(store::base_store::StoreError::RootMissing { kerns: kerns.len() });
		}
		let mut g = GraphGnn::new();
		g.data_dir = dir.to_string();
		g.set_store(store);
		g.set_flushed_epoch(loaded_epoch);
		return Ok(g);
	}

	let root = kerns
		.get("root")
		.cloned()
		.expect("root presence checked above");
	let mut g = GraphGnn::from_saved_with_mode(
		root,
		network_id,
		dir.to_string(),
		kerns,
		std::collections::HashSet::new(),
		quant_mode,
	);
	g.set_store(store);
	g.set_flushed_epoch(loaded_epoch);
	Ok(g)
}

// The identity of the vectors about to be written. `None` until BOTH halves are
// known: the configured model (bound at open) and a dimension the graph actually
// holds. Stamping a dimension of 0 on an empty store would make the first real
// ingest look like a model change.
fn stamp_of(g: &GraphGnn) -> Option<store::base_store::EmbedStamp> {
	let model = g.embed_model();
	if model.is_empty() {
		return None;
	}
	Some(store::base_store::EmbedStamp {
		model: model.to_string(),
		dim: g.entity_vector_dim()?,
	})
}

// Fail-open: a stamp that cannot be read or written must never block a save.
fn check_stamp(store: &store::base_store::Store, stamp: Option<&store::base_store::EmbedStamp>) {
	let Some(stamp) = stamp else {
		return;
	};
	if let Err(e) = store.check_embed_stamp(stamp) {
		tracing::warn!(target: "kern.store", error = %e, "embedding stamp check failed; continuing");
	}
}

// Call once a graph is bound to its configured model, so a model swap is caught
// at open instead of at the first save.
pub fn check_graph_stamp(g: &GraphGnn) {
	if let Some(store) = g.store() {
		check_stamp(&store, stamp_of(g).as_ref());
	}
}

pub fn merged_root(g: &GraphGnn) -> Kern {
	let root_id = g.root.id.clone();
	let mut merged = g
		.map()
		.get(&root_id)
		.cloned()
		.unwrap_or_else(|| g.root.clone());
	merged.id = g.root.id.clone();
	merged.root_id = g.root.root_id.clone();
	merged.graviton_text = g.root.graviton_text.clone();
	merged.graviton_vec = g.root.graviton_vec.clone();
	merged.inner_radius = g.root.inner_radius;
	merged.outer_radius = g.root.outer_radius;
	// REPLACE, don't union: a union re-adds claim kinds from the stale map base,
	// so a removal on g.root (`claim-kind rm`) never persists. The parent edges
	// follow the same rule for the same reason.
	merged.claim_kinds = g.root.claim_kinds.clone();
	merged.claim_kind_parents = g.root.claim_kind_parents.clone();
	merged
}

pub fn save_graph_into(
	store: &store::base_store::Store,
	g: &GraphGnn,
) -> Result<(), store::base_store::StoreError> {
	check_stamp(store, stamp_of(g).as_ref());
	let mut kerns = g.map().clone();
	kerns.insert(g.root.id.clone(), merged_root(g));
	store.save_all_kerns(&kerns, &g.network_id, g.quant_mode, g.unloaded_ids())?;
	Ok(())
}

pub fn flush_guarded(
	g: &GraphGnn,
	expected: u64,
) -> Result<store::base_store::FlushOutcome, store::base_store::StoreError> {
	match g.store() {
		Some(store) => {
			check_stamp(&store, stamp_of(g).as_ref());
			let mut kerns = g.map().clone();
			kerns.insert(g.root.id.clone(), merged_root(g));
			store.flush_guarded(
				&kerns,
				&g.network_id,
				g.quant_mode,
				expected,
				g.unloaded_ids(),
			)
		}
		None => Ok(store::base_store::FlushOutcome::Flushed { epoch: expected }),
	}
}

// Cloned under the read guard so the graph lock can be DROPPED before the flush txn.
pub struct FlushSnapshot {
	store: std::sync::Arc<store::base_store::Store>,
	kerns: HashMap<String, Kern>,
	network_id: String,
	quant_mode: QuantizationMode,
	stamp: Option<store::base_store::EmbedStamp>,
	// Unloaded-kern ids at snapshot time; the flush prune must spare their rows.
	unloaded: std::collections::HashSet<String>,
}

// Call under the read guard; drop it before flush_snapshot runs.
pub fn snapshot_for_flush(g: &GraphGnn) -> Option<FlushSnapshot> {
	let store = g.store()?;
	let mut kerns = g.map().clone();
	kerns.insert(g.root.id.clone(), merged_root(g));
	Some(FlushSnapshot {
		store,
		kerns,
		network_id: g.network_id.clone(),
		quant_mode: g.quant_mode,
		stamp: stamp_of(g),
		unloaded: g.unloaded_ids().clone(),
	})
}

// No graph lock held. The epoch check runs inside the store write txn, so
// `expected` still holds.
pub fn flush_snapshot(
	snap: &FlushSnapshot,
	expected: u64,
) -> Result<store::base_store::FlushOutcome, store::base_store::StoreError> {
	check_stamp(&snap.store, snap.stamp.as_ref());
	snap.store.flush_guarded(
		&snap.kerns,
		&snap.network_id,
		snap.quant_mode,
		expected,
		&snap.unloaded,
	)
}

// save_all_kerns prunes rows outside the live set — minus the unloaded ids,
// whose disk rows are residency, not garbage — so no deregistered kern can
// resurrect while an idle-unloaded one survives the flush.
pub fn save_all(g: &GraphGnn) -> Result<(), store::base_store::StoreError> {
	match g.store() {
		Some(store) => save_graph_into(&store, g),
		None => Ok(()),
	}
}

// On-disk vectors are always int8; target_mode sets only the HNSW rebuild mode.
pub fn compress_dir(
	src: &str,
	out_dir: &str,
	target_mode: QuantizationMode,
) -> Result<(), store::base_store::StoreError> {
	let mut g = load_dir(src)?;
	g.quant_mode = target_mode;
	let dest = store::base_store::Store::open(out_dir)?;
	save_graph_into(&dest, &g)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::graph::GraphGnn;
	use tempfile::tempdir;

	#[test]
	fn merged_root_overlays_authoritative_fields_over_stale_map_entry() {
		let mut g = GraphGnn::new();
		let mut stale = g.root.clone();
		stale.graviton_text = String::new();
		stale.claim_kinds.clear();
		g.register(stale);
		g.root.graviton_text = "guiding purpose".to_string();
		g.root
			.claim_kinds
			.insert("chat".to_string(), "desc".to_string());

		let merged = merged_root(&g);
		assert_eq!(merged.id, g.root.id);
		assert_eq!(merged.graviton_text, "guiding purpose");
		assert_eq!(
			merged.claim_kinds.get("chat").map(String::as_str),
			Some("desc")
		);
	}

	#[test]
	fn rows_without_root_error_instead_of_loading_empty() {
		// Regression for the wiped-store bug: a bad read that saw kern rows but
		// no root used to return an EMPTY graph stamped with the store's live
		// epoch. Reconcile then saw nothing stale and the first dirty flush
		// overwrote every row on disk with nothing. It must be an error.
		use store::base_store::{Store, StoreError};
		let dir = tempdir().unwrap();
		let d = dir.path().to_string_lossy().to_string();
		let store = Store::open(&d).unwrap();
		store.save_one_kern(&Kern::new("orphan", "root")).unwrap();
		drop(store);

		match load_dir(&d) {
			Err(StoreError::RootMissing { kerns }) => assert_eq!(kerns, 1),
			Err(e) => panic!("wrong error for rootless non-empty store: {e}"),
			Ok(_) => panic!("rootless non-empty store must refuse to load"),
		}
	}

	#[test]
	fn a_truly_empty_store_still_loads_as_a_fresh_graph() {
		let dir = tempdir().unwrap();
		let d = dir.path().to_string_lossy().to_string();
		let g = load_dir(&d).expect("an empty store is a fresh store, not an error");
		assert!(g.loaded("root").is_some() || g.map().is_empty());
	}

	#[test]
	fn an_unloaded_kern_survives_a_full_save_and_reload() {
		// Regression for the idle-unload wipe: unload removes a kern from the
		// resident map (residency, not forgetting), and the next full save's
		// destructive prune used to delete its disk row — the only copy —
		// permanently losing its thoughts while their reason edges lived on.
		use base::base_types::{mk_entity, EntityKind};
		use store::base_store::Store;
		let dir = tempdir().unwrap();
		let d = dir.path().to_string_lossy().to_string();

		let mut g = GraphGnn::new();
		g.data_dir = d.clone();
		g.set_store(std::sync::Arc::new(Store::open(&d).unwrap()));
		let mut k = Kern::new("idle-kern", &g.root.id);
		k.entities.insert(
			"t1".to_string(),
			mk_entity("t1", "a thought", 0.5, EntityKind::Claim),
		);
		g.register(k);

		g.unload("idle-kern").unwrap();
		assert!(g.is_unloaded("idle-kern"), "unload parks the kern off-RAM");

		save_all(&g).unwrap();
		drop(g);

		let g2 = load_dir(&d).unwrap();
		let k2 = g2
			.map()
			.get("idle-kern")
			.expect("the unloaded kern's disk row survives the flush prune");
		assert!(
			k2.entities.contains_key("t1"),
			"and its thoughts reload with it"
		);
	}
}
