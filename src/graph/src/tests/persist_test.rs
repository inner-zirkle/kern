//! Tests extracted from persist.rs
#![allow(unused)]
use super::*;

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
		use store_core::{Store, StoreError};
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
		use store_core::Store;
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
