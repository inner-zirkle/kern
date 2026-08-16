//! Tests extracted from merge.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use crate::graph::GraphGnn;
	use base::base_types::{mk_entity, EntityKind, Kern};
	use std::time::{Duration, UNIX_EPOCH};

	fn t(secs: u64) -> Option<SystemTime> {
		Some(UNIX_EPOCH + Duration::from_secs(secs))
	}

	#[test]
	fn merge_is_monotonic() {
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		let remote = mk_entity("e1", "x", 5.0, EntityKind::Fact);
		let changed = merge_entity(&mut local, &remote);
		assert!(changed);
		assert_eq!(local.heat, 5.0);

		let mut local = mk_entity("e1", "x", 5.0, EntityKind::Fact);
		let remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		let changed = merge_entity(&mut local, &remote);
		assert!(!changed);
		assert_eq!(local.heat, 5.0);
	}

	#[test]
	fn merge_is_idempotent() {
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		let mut remote = mk_entity("e1", "x", 5.0, EntityKind::Fact);
		remote.access_count.increment("b", 2);
		remote.accessed_at = t(100);
		remote.created_at = t(10);

		assert!(merge_entity(&mut local, &remote));
		let snap_heat = local.heat;
		let snap_alpha = local.conf_alpha;
		let snap_ac = local.access_count.value();
		let snap_acc = local.accessed_at;
		let snap_created = local.created_at;
		let snap_score = local.score;

		let changed = merge_entity(&mut local, &remote);
		assert!(!changed);
		assert_eq!(local.heat, snap_heat);
		assert_eq!(local.conf_alpha, snap_alpha);
		assert_eq!(local.access_count.value(), snap_ac);
		assert_eq!(local.accessed_at, snap_acc);
		assert_eq!(local.created_at, snap_created);
		assert_eq!(local.score, snap_score);
	}

	#[test]
	fn merge_does_not_import_confidence() {
		// SECURITY regression guard: the confidence-by-max poisoning pin.
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		let local_alpha = local.conf_alpha;
		let local_beta = local.conf_beta;
		let local_mean = local.conf_mean();

		let mut poisoned = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		poisoned.conf_alpha = 1.0e9;
		poisoned.conf_beta = 0.0;

		merge_entity(&mut local, &poisoned);

		assert_eq!(
			local.conf_alpha, local_alpha,
			"remote alpha must not be imported"
		);
		assert_eq!(
			local.conf_beta, local_beta,
			"remote beta must not be imported"
		);
		assert_eq!(
			local.conf_mean(),
			local_mean,
			"confidence stays replica-local"
		);
	}

	#[test]
	fn merge_joins_access_count() {
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		local.access_count.increment("a", 1);
		let mut remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		remote.access_count.increment("b", 2);
		merge_entity(&mut local, &remote);
		assert_eq!(local.access_count.value(), 3);
	}

	#[test]
	fn merge_status_superseded_dominates() {
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		let mut remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		remote.status = EntityStatus::Superseded;
		let changed = merge_entity(&mut local, &remote);
		assert!(changed);
		assert_eq!(local.status, EntityStatus::Superseded);

		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		local.status = EntityStatus::Superseded;
		let remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		merge_entity(&mut local, &remote);
		assert_eq!(local.status, EntityStatus::Superseded);
	}

	#[test]
	fn merge_created_at_takes_earliest_accessed_latest() {
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		local.created_at = t(100);
		local.accessed_at = t(100);
		let mut remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		remote.created_at = t(50);
		remote.accessed_at = t(200);
		merge_entity(&mut local, &remote);
		assert_eq!(local.created_at, t(50), "created_at joins to the min");
		assert_eq!(local.accessed_at, t(200), "accessed_at joins to the max");
	}

	#[test]
	fn merge_entity_into_inserts_then_merges() {
		let mut g = GraphGnn::new();
		let fallback = g.root.id.clone();

		let remote = mk_entity("eX", "x", 1.0, EntityKind::Fact);
		let changed = merge_entity_into(&mut g, &fallback, remote);
		assert!(changed);
		assert!(g.kerns.get(&fallback).unwrap().entities.contains_key("eX"));
		assert_eq!(g.kern_of_entity("eX"), Some(fallback.as_str()));

		let remote2 = mk_entity("eX", "x", 9.0, EntityKind::Fact);
		let changed = merge_entity_into(&mut g, &fallback, remote2);
		assert!(changed);

		let total: usize = g
			.kerns
			.values()
			.filter(|k| k.entities.contains_key("eX"))
			.count();
		assert_eq!(total, 1);
		assert_eq!(
			g.kerns
				.get(&fallback)
				.unwrap()
				.entities
				.get("eX")
				.unwrap()
				.heat,
			9.0
		);
	}

	#[test]
	fn merge_to_superseded_drops_entity_from_search_index() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();
		let mut local = mk_entity("eX", "x", 1.0, EntityKind::Fact);
		local.vector = vec![1.0, 0.0].into();
		local.status = EntityStatus::Active;
		g.entity_idx.insert("eX".into(), vec![1.0, 0.0].into());
		g.kerns
			.get_mut(&kid)
			.unwrap()
			.entities
			.insert("eX".into(), local);
		g.index_entity("eX", &kid);

		let before: Vec<String> = crate::search::search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			before.contains(&"eX".to_string()),
			"active entity indexed before merge"
		);

		let mut remote = mk_entity("eX", "x", 1.0, EntityKind::Fact);
		remote.status = EntityStatus::Superseded;
		merge_entity_into(&mut g, &kid, remote);

		assert_eq!(
			g.kerns
				.get(&kid)
				.unwrap()
				.entities
				.get("eX")
				.unwrap()
				.status,
			EntityStatus::Superseded,
			"CRDT join propagated Superseded",
		);
		let after: Vec<String> = crate::search::search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			!after.contains(&"eX".to_string()),
			"merge-superseded entity removed from search index"
		);
	}

	#[test]
	fn merged_entity_is_vector_searchable_without_rebuild() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();

		let mut remote = mk_entity("eV", "remote thought", 1.0, EntityKind::Fact);
		remote.vector = vec![0.0, 1.0].into();
		remote.gnn_vector = vec![1.0, 0.0].into();
		assert!(merge_entity_into(&mut g, &kid, remote));

		let hits: Vec<String> = crate::search::search_all_unlocked(&g, &[0.0, 1.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			hits.contains(&"eV".to_string()),
			"merged entity must be returned by vector search without rebuild_index"
		);
		assert!(
			g.gnn_entity_idx
				.search(&[1.0, 0.0], 5, 50)
				.iter()
				.any(|h| h.id == "eV"),
			"merged entity's gnn vector indexed on receipt"
		);
	}

	#[test]
	fn merged_superseded_entity_is_stored_but_not_indexed() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();

		let mut remote = mk_entity("eS", "dead on arrival", 1.0, EntityKind::Fact);
		remote.vector = vec![0.0, 1.0].into();
		remote.status = EntityStatus::Superseded;
		assert!(merge_entity_into(&mut g, &kid, remote));

		assert!(g.kerns.get(&kid).unwrap().entities.contains_key("eS"));
		let hits: Vec<String> = crate::search::search_all_unlocked(&g, &[0.0, 1.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			!hits.contains(&"eS".to_string()),
			"a superseded entity never enters the search index"
		);
	}

	#[test]
	fn a_merge_cannot_hijack_an_id_owned_by_another_kern() {
		// SECURITY regression guard: a forged id colliding with a local-origin
		// entity must not merge into it or repoint the global index.
		let mut g = GraphGnn::new();
		let local_kern = g.root.id.clone();
		assert!(merge_entity_into(
			&mut g,
			&local_kern,
			mk_entity("eX", "real", 1.0, EntityKind::Fact)
		));

		let phantom = "remote-netA-k1";
		g.register(Kern::new(phantom, &g.root.id));

		let mut forged = mk_entity("eX", "real", 9.0, EntityKind::Fact);
		forged.status = EntityStatus::Superseded;
		let changed = merge_entity_into(&mut g, phantom, forged);

		assert!(!changed, "hijack must be rejected");
		let local = g
			.kerns
			.get(&local_kern)
			.unwrap()
			.entities
			.get("eX")
			.unwrap();
		assert_eq!(local.status, EntityStatus::Active, "local status untouched");
		assert_eq!(local.heat, 1.0, "local heat untouched");
		assert!(
			!g.kerns.get(phantom).unwrap().entities.contains_key("eX"),
			"phantom kern must not gain the hijacked id"
		);
		assert_eq!(
			g.kern_of_entity("eX"),
			Some(local_kern.as_str()),
			"global index still points at the local owner"
		);
	}

	#[test]
	fn a_local_kern_merge_keeps_the_trusted_disk_absorb_path_intact() {
		// `absorb_graph` folds disk rows through the same entry point; only the
		// `remote-*` phantom kerns are untrusted.
		let mut g = GraphGnn::new();
		let local_kern = g.root.id.clone();
		let mut row = mk_entity("eD", "x", 3.0, EntityKind::Fact);
		row.access_count.increment("a", 4);
		assert!(merge_entity_into(&mut g, &local_kern, row));

		let e = g
			.kerns
			.get(&local_kern)
			.unwrap()
			.entities
			.get("eD")
			.unwrap();
		assert_eq!(e.heat, 3.0, "a local-kern row keeps its heat");
		assert_eq!(e.access_count.value(), 4);
	}

	#[test]
	fn merge_reason_lww_score_and_joins_traversal_idempotently() {
		let mut local = Reason {
			score: 0.3,
			score_lamport: 1,
			score_producer: "r1".into(),
			..Default::default()
		};
		local.traversal_count.increment("a", 1);
		let mut remote = Reason {
			score: 0.7,
			score_lamport: 2,
			score_producer: "r2".into(),
			..Default::default()
		};
		remote.traversal_count.increment("b", 2);

		assert!(merge_reason(&mut local, &remote));
		assert_eq!(local.score, 0.7, "higher lamport wins the LWW-Register");
		assert_eq!(local.score_lamport, 2);
		assert_eq!(local.traversal_count.value(), 3, "traversal GCounters join");

		assert!(!merge_reason(&mut local, &remote));
		assert_eq!(local.score, 0.7);
		assert_eq!(local.traversal_count.value(), 3);

		let lower = Reason {
			score: 0.1,
			score_lamport: 1,
			score_producer: "r1".into(),
			..Default::default()
		};
		assert!(
			!merge_reason(&mut local, &lower),
			"lower lamport does not overwrite"
		);
		assert_eq!(local.score, 0.7);

		let same_lamport_higher_producer = Reason {
			score: 0.9,
			score_lamport: 2,
			score_producer: "r9".into(),
			..Default::default()
		};
		assert!(
			merge_reason(&mut local, &same_lamport_higher_producer),
			"same lamport, higher producer wins"
		);
		assert_eq!(local.score, 0.9);
	}

	#[test]
	fn superseded_by_join_picks_the_lexicographically_higher_id() {
		let mut a = String::from("idA");
		assert!(join_superseded_by(&mut a, "idZ"));
		assert_eq!(a, "idZ");
		assert!(!join_superseded_by(&mut a, "idB"));
		assert_eq!(a, "idZ");
		assert!(!join_superseded_by(&mut a, ""));
		assert_eq!(a, "idZ");
	}

	#[test]
	fn merge_entity_never_imports_replica_local_mutable_state() {
		// Field-addition guard: keep in sync when adding mutable Entity fields.
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		let snap_alpha = local.conf_alpha;
		let snap_beta = local.conf_beta;
		let snap_unlinked = local.unlinked_count;

		let mut remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		remote.conf_alpha = 1.0e9;
		remote.conf_beta = 1.0e9;
		remote.unlinked_count = 9_999;

		merge_entity(&mut local, &remote);

		assert_eq!(
			local.conf_alpha, snap_alpha,
			"conf_alpha stays replica-local"
		);
		assert_eq!(local.conf_beta, snap_beta, "conf_beta stays replica-local");
		assert_eq!(
			local.unlinked_count, snap_unlinked,
			"unlinked_count is local bookkeeping"
		);
	}

	#[test]
	fn merge_entity_never_imports_statements() {
		let mut local = mk_entity("e1", "a", 1.0, EntityKind::Fact);
		let mut remote = mk_entity("e1", "b", 1.0, EntityKind::Fact);
		remote.statements = vec!["b".into(), "c".into()];
		merge_entity(&mut local, &remote);
		assert_eq!(
			local.statements,
			vec!["a".to_string()],
			"statements are content-addressed by id and never join from a remote"
		);
	}

	#[test]
	fn cleared_statements_do_not_resurrect_under_merge() {
		let mut local = mk_entity("e1", "a", 1.0, EntityKind::Fact);
		let remote = mk_entity("e1", "a", 1.0, EntityKind::Fact);
		local.set_text("replacement".into());
		assert!(local.statements.is_empty());
		for _ in 0..3 {
			merge_entity(&mut local, &remote);
		}
		assert!(
			local.statements.is_empty(),
			"a locally cleared statement stays cleared across repeated merge rounds"
		);
	}

	fn converged(order: &[usize], remotes: &[Entity]) -> Entity {
		let mut local = mk_entity("e1", "a", 0.0, EntityKind::Fact);
		for &i in order {
			merge_entity(&mut local, &remotes[i]);
		}
		local
	}

	fn state(e: &Entity) -> (Vec<String>, u64, String, Option<SystemTime>, f32, u64) {
		(
			e.statements.clone(),
			e.valid_until_lamport,
			e.valid_until_producer.clone(),
			e.valid_until,
			e.heat,
			e.access_count.value(),
		)
	}

	fn lww_remote(lamport: u64, producer: &str, secs: u64, heat: f64) -> Entity {
		let mut r = mk_entity("e1", "a", heat, EntityKind::Fact);
		r.valid_until = Some(UNIX_EPOCH + Duration::from_secs(secs));
		r.valid_until_lamport = lamport;
		r.valid_until_producer = producer.into();
		r.access_count.increment(producer, lamport);
		r
	}

	#[test]
	fn merge_entity_is_order_independent_and_idempotent() {
		let remotes = [
			lww_remote(2, "r1", 100, 0.5),
			lww_remote(5, "r2", 200, 0.9),
			lww_remote(5, "r1", 300, 0.2),
		];
		let baseline = state(&converged(&[0, 1, 2], &remotes));
		for order in [
			[0, 2, 1],
			[1, 0, 2],
			[1, 2, 0],
			[2, 0, 1],
			[2, 1, 0],
			[0, 1, 2],
		] {
			assert_eq!(
				state(&converged(&order, &remotes)),
				baseline,
				"every permutation converges to the same state: {order:?}"
			);
		}
		// duplicated and repeated delivery changes nothing
		assert_eq!(
			state(&converged(&[0, 1, 2, 1, 0, 2, 2, 1], &remotes)),
			baseline,
			"merge is idempotent under duplicate delivery"
		);
	}

	#[test]
	fn merge_entity_second_apply_reports_no_change() {
		let mut local = mk_entity("e1", "a", 0.0, EntityKind::Fact);
		let remote = lww_remote(4, "r1", 100, 0.7);
		assert!(merge_entity(&mut local, &remote));
		assert!(
			!merge_entity(&mut local, &remote),
			"re-applying the same delta is a no-op"
		);
	}

	#[test]
	fn merge_entity_valid_until_lww_takes_higher_lamport() {
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		local.valid_until = Some(UNIX_EPOCH + Duration::from_secs(100));
		local.valid_until_lamport = 1;
		local.valid_until_producer = "r1".into();

		let mut remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		remote.valid_until = Some(UNIX_EPOCH + Duration::from_secs(50));
		remote.valid_until_lamport = 2;
		remote.valid_until_producer = "r2".into();

		assert!(merge_entity(&mut local, &remote));
		assert_eq!(
			local.valid_until,
			Some(UNIX_EPOCH + Duration::from_secs(50)),
			"higher lamport wins, not min time"
		);
		assert_eq!(local.valid_until_lamport, 2);
	}

	#[test]
	fn merge_entity_valid_until_lower_lamport_loses() {
		let mut local = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		local.valid_until = Some(UNIX_EPOCH + Duration::from_secs(100));
		local.valid_until_lamport = 5;
		local.valid_until_producer = "r1".into();

		let mut remote = mk_entity("e1", "x", 1.0, EntityKind::Fact);
		remote.valid_until = Some(UNIX_EPOCH + Duration::from_secs(50));
		remote.valid_until_lamport = 2;
		remote.valid_until_producer = "r2".into();

		assert!(!merge_entity(&mut local, &remote));
		assert_eq!(
			local.valid_until,
			Some(UNIX_EPOCH + Duration::from_secs(100)),
			"the lower lamport loses; the local TTL stands"
		);
		assert_eq!(local.valid_until_lamport, 5);
	}
}
