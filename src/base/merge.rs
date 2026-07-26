use std::sync::atomic::{AtomicU64, Ordering};

use crate::base::log_throttle::LogThrottle;
use std::time::SystemTime;

use crate::base::constants;
use crate::base::graph::GraphGnn;
use crate::base::types::{Entity, EntityStatus, Reason};
use crate::crdt::{lww_wins, GCounter};

// Gossip files every peer's rows into a `remote-<network_id>-<kern_id>` phantom kern
// (see `gossip::handler`), so the kern id is the one durable "this came off the wire"
// signal an entity carries.
pub fn is_remote_kern_id(kern_id: &str) -> bool {
	kern_id.starts_with("remote-")
}

// SECURITY: federation is unauthenticated, so every ranking signal a peer can set is
// dropped at the boundary. Heat and access are earned by LOCAL use only — a remote
// assertion must never raise either — and confidence resets to the neutral prior
// rather than arriving pre-trusted. Without this the first-contact insert below
// admits attacker-chosen values verbatim, bypassing merge_entity's join guards.
fn strip_untrusted_ranking_signals(e: &mut Entity) {
	e.heat = 0.0;
	e.heat_updated_at = None;
	e.access_count = GCounter::new();
	e.accessed_at = None;
	e.conf_alpha = 0.0;
	e.conf_beta = 0.0;
	e.refresh_score();
}

fn join_time(
	local: &mut Option<SystemTime>,
	remote: Option<SystemTime>,
	take: impl Fn(SystemTime, SystemTime) -> bool,
) -> bool {
	match (*local, remote) {
		(_, None) => false,
		(None, Some(r)) => {
			*local = Some(r);
			true
		}
		(Some(l), Some(r)) if take(r, l) => {
			*local = Some(r);
			true
		}
		_ => false,
	}
}

fn join_max_time(local: &mut Option<SystemTime>, remote: Option<SystemTime>) -> bool {
	join_time(local, remote, |r, l| r > l)
}

fn join_min_time(local: &mut Option<SystemTime>, remote: Option<SystemTime>) -> bool {
	join_time(local, remote, |r, l| r < l)
}

fn join_lww_time(
	local: &mut Option<SystemTime>,
	local_lamport: &mut u64,
	local_producer: &mut String,
	remote: Option<SystemTime>,
	remote_lamport: u64,
	remote_producer: &str,
) -> bool {
	if lww_wins(
		(remote_lamport, remote_producer),
		(*local_lamport, local_producer.as_str()),
	) {
		*local = remote;
		*local_lamport = remote_lamport;
		*local_producer = remote_producer.to_string();
		true
	} else {
		false
	}
}

fn join_superseded_by(local: &mut String, remote: &str) -> bool {
	if !remote.is_empty() && remote > local.as_str() {
		*local = remote.to_string();
		true
	} else {
		false
	}
}

pub fn merge_entity(local: &mut Entity, remote: &Entity) -> bool {
	let mut changed = local.access_count.merge(&remote.access_count);
	if remote.heat > local.heat {
		local.heat = remote.heat;
		changed = true;
	}
	// SECURITY: conf_alpha/conf_beta/unlinked_count are never imported from remote
	// — a max-join on confidence is an irreversible poisoning pin.
	if remote.status == EntityStatus::Superseded && local.status != EntityStatus::Superseded {
		local.status = EntityStatus::Superseded;
		changed = true;
	}
	changed |= join_superseded_by(&mut local.superseded_by, &remote.superseded_by);
	changed |= join_min_time(&mut local.created_at, remote.created_at);
	changed |= join_max_time(&mut local.accessed_at, remote.accessed_at);
	changed |= join_max_time(&mut local.updated_at, remote.updated_at);
	changed |= join_max_time(&mut local.heat_updated_at, remote.heat_updated_at);
	changed |= join_lww_time(
		&mut local.valid_until,
		&mut local.valid_until_lamport,
		&mut local.valid_until_producer,
		remote.valid_until,
		remote.valid_until_lamport,
		&remote.valid_until_producer,
	);
	// SECURITY: statements are never imported from remote. `id == content_hash(text)`
	// and `statements == [text]`, so a same-id peer has identical content by construction
	// and a differing one is asserting content its id does not hash to. Unioning it both
	// breaks content-addressing and makes a locally-cleared statement resurrect.
	if changed {
		local.refresh_score();
	}
	changed
}

pub fn merge_reason(local: &mut Reason, remote: &Reason) -> bool {
	let mut changed = local.traversal_count.merge(&remote.traversal_count);
	if lww_wins(
		(remote.score_lamport, &remote.score_producer),
		(local.score_lamport, &local.score_producer),
	) {
		local.score = remote.score;
		local.score_lamport = remote.score_lamport;
		local.score_producer = remote.score_producer.clone();
		changed = true;
	}
	changed
}

const REMOTE_CAP_WARN_SECS: u64 = 300;
static REMOTE_CAP_DROPPED: AtomicU64 = AtomicU64::new(0);
static REMOTE_CAP_WARN: LogThrottle = LogThrottle::new(REMOTE_CAP_WARN_SECS);

// New remote ids refused because their phantom kern is at the entity cap. Known
// ids still merge, so nothing else distinguishes a capped peer from a quiet one.
pub fn remote_cap_dropped() -> u64 {
	REMOTE_CAP_DROPPED.load(Ordering::Relaxed)
}

// SECURITY: id owned by a DIFFERENT kern → reject (hijack); owned by none →
// insert under a per-kern cap; already in target → CRDT-merge.
pub fn merge_remote_entity(g: &mut GraphGnn, target_kern_id: &str, mut remote: Entity) -> bool {
	if is_remote_kern_id(target_kern_id) {
		strip_untrusted_ranking_signals(&mut remote);
	}
	let host = g
		.kerns
		.iter()
		.find(|(_, k)| k.entities.contains_key(&remote.id))
		.map(|(kid, _)| kid.clone());
	match host {
		Some(kid) if kid == target_kern_id => {
			let (changed, now_superseded) = match g.kerns.get_mut(&kid) {
				Some(kern) => match kern.entities.get_mut(&remote.id) {
					Some(local) => {
						let changed = merge_entity(local, &remote);
						(changed, local.status == EntityStatus::Superseded)
					}
					None => (false, false),
				},
				None => (false, false),
			};
			// A join that flipped to Superseded must evict from the ANN indices —
			// same invariant as `accept::supersede`: superseded is never a valid result.
			if now_superseded {
				g.entity_idx.delete(&remote.id);
				g.gnn_entity_idx.delete(&remote.id);
			}
			changed
		}
		Some(other) => {
			tracing::warn!(
				target: "kern.merge",
				id = %crate::base::util::short_id(&remote.id),
				owner = %other,
				target = %target_kern_id,
				"remote entity id collides with an entity owned by another kern; rejected"
			);
			false
		}
		None => {
			let Some(kern) = g.kerns.get_mut(target_kern_id) else {
				tracing::warn!(target: "kern.merge", kern = %target_kern_id, "merge_remote_entity: target kern missing; entity dropped");
				return false;
			};
			if kern.entities.len() >= constants::GOSSIP_REMOTE_KERN_ENTITY_CAP {
				// Known ids keep merging above the cap; only NEW ones are refused, so a
				// peer at the ceiling looks healthy while its fresh knowledge silently
				// never arrives. Unthrottled this warned once per dropped entity — a
				// gossiping peer would flood the log it is supposed to be visible in.
				let total = REMOTE_CAP_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
				if REMOTE_CAP_WARN.allow() {
					tracing::warn!(
						target: "kern.merge",
						kern = %target_kern_id,
						cap = constants::GOSSIP_REMOTE_KERN_ENTITY_CAP,
						total_dropped = total,
						"remote kern at entity cap; dropping new remote entity \
						 (further drops counted, not logged)"
					);
				}
				return false;
			}
			let id = remote.id.clone();
			// Index on insert (mirrors `accept::commit_entity`) or the entity is
			// invisible to vector search until a rebuild; Superseded is stored, not indexed.
			let searchable = remote.status != EntityStatus::Superseded;
			let vector = searchable
				.then(|| remote.vector.clone())
				.filter(|v| !v.is_empty());
			let gnn_vector = searchable
				.then(|| remote.gnn_vector.clone())
				.filter(|v| !v.is_empty());
			kern.entities.insert(id.clone(), remote);
			g.index_entity(&id, target_kern_id);
			if let Some(v) = vector {
				g.entity_idx.insert(id.clone(), v);
			}
			if let Some(v) = gnn_vector {
				g.gnn_entity_idx.insert(id.clone(), v);
			}
			true
		}
	}
}

// Fold a disk-loaded graph into the live one after a refused stale flush: the
// live graph keeps its unflushed rows, the external writer's rows join via the
// same CRDT joins gossip uses, and the caller retries the flush with the disk
// epoch. Kern-shell fields (graviton, radii, weights) stay local for kerns both
// sides know — only rows and topology union in.
pub fn absorb_graph(local: &mut GraphGnn, disk: GraphGnn) -> usize {
	let mut changed = 0;
	for (kid, mut dkern) in disk.kerns {
		let entities = std::mem::take(&mut dkern.entities);
		let reasons = std::mem::take(&mut dkern.reasons);
		let refs = std::mem::take(&mut dkern.refs);
		let sources = std::mem::take(&mut dkern.source_index);
		let claim_kinds = std::mem::take(&mut dkern.claim_kinds);
		let claim_kind_parents = std::mem::take(&mut dkern.claim_kind_parents);
		match local.kerns.get_mut(&kid) {
			Some(lkern) => {
				for c in &dkern.children {
					if !lkern.children.contains(c) {
						lkern.children.push(c.clone());
					}
				}
			}
			None => {
				dkern.by_from.clear();
				dkern.by_to.clear();
				local.kerns.insert(kid.clone(), dkern);
				changed += 1;
			}
		}
		for e in entities.into_values() {
			if merge_remote_entity(local, &kid, e) {
				changed += 1;
			}
		}
		let Some(lkern) = local.kerns.get_mut(&kid) else {
			continue;
		};
		for (rid, r) in reasons {
			match lkern.reasons.get_mut(&rid) {
				Some(lr) => {
					if merge_reason(lr, &r) {
						changed += 1;
					}
				}
				None => {
					crate::base::reason::add_reason(lkern, r);
					changed += 1;
				}
			}
		}
		for (k, v) in refs {
			lkern.refs.entry(k).or_insert(v);
		}
		for (k, v) in sources {
			lkern.source_index.entry(k).or_insert(v);
		}
		for (k, v) in claim_kinds {
			lkern.claim_kinds.entry(k).or_insert(v);
		}
		for (k, v) in claim_kind_parents {
			lkern.claim_kind_parents.entry(k).or_insert(v);
		}
	}
	changed
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base::graph::GraphGnn;
	use crate::base::types::{mk_entity, EntityKind, Kern};
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
	fn merge_does_not_import_remote_confidence() {
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
	fn merge_remote_entity_inserts_then_merges() {
		let mut g = GraphGnn::new();
		let fallback = g.root.id.clone();

		let remote = mk_entity("eX", "x", 1.0, EntityKind::Fact);
		let changed = merge_remote_entity(&mut g, &fallback, remote);
		assert!(changed);
		assert!(g.kerns.get(&fallback).unwrap().entities.contains_key("eX"));
		assert_eq!(g.kern_of_entity("eX"), Some(fallback.as_str()));

		let remote2 = mk_entity("eX", "x", 9.0, EntityKind::Fact);
		let changed = merge_remote_entity(&mut g, &fallback, remote2);
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

		let before: Vec<String> = crate::base::search::search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			before.contains(&"eX".to_string()),
			"active entity indexed before merge"
		);

		let mut remote = mk_entity("eX", "x", 1.0, EntityKind::Fact);
		remote.status = EntityStatus::Superseded;
		merge_remote_entity(&mut g, &kid, remote);

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
		let after: Vec<String> = crate::base::search::search_all_unlocked(&g, &[1.0, 0.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			!after.contains(&"eX".to_string()),
			"merge-superseded entity removed from search index"
		);
	}

	#[test]
	fn merged_remote_entity_is_vector_searchable_without_rebuild() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();

		let mut remote = mk_entity("eV", "remote thought", 1.0, EntityKind::Fact);
		remote.vector = vec![0.0, 1.0].into();
		remote.gnn_vector = vec![1.0, 0.0].into();
		assert!(merge_remote_entity(&mut g, &kid, remote));

		let hits: Vec<String> = crate::base::search::search_all_unlocked(&g, &[0.0, 1.0], 5)
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
	fn merged_superseded_remote_entity_is_stored_but_not_indexed() {
		let mut g = GraphGnn::new();
		let kid = g.root.id.clone();

		let mut remote = mk_entity("eS", "dead on arrival", 1.0, EntityKind::Fact);
		remote.vector = vec![0.0, 1.0].into();
		remote.status = EntityStatus::Superseded;
		assert!(merge_remote_entity(&mut g, &kid, remote));

		assert!(g.kerns.get(&kid).unwrap().entities.contains_key("eS"));
		let hits: Vec<String> = crate::base::search::search_all_unlocked(&g, &[0.0, 1.0], 5)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();
		assert!(
			!hits.contains(&"eS".to_string()),
			"a superseded entity never enters the search index"
		);
	}

	#[test]
	fn remote_cannot_hijack_id_owned_by_another_kern() {
		// SECURITY regression guard: a forged id colliding with a local-origin
		// entity must not merge into it or repoint the global index.
		let mut g = GraphGnn::new();
		let local_kern = g.root.id.clone();
		assert!(merge_remote_entity(
			&mut g,
			&local_kern,
			mk_entity("eX", "real", 1.0, EntityKind::Fact)
		));

		let phantom = "remote-netA-k1";
		g.register(Kern::new(phantom, &g.root.id));

		let mut forged = mk_entity("eX", "real", 9.0, EntityKind::Fact);
		forged.status = EntityStatus::Superseded;
		let changed = merge_remote_entity(&mut g, phantom, forged);

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
	fn remote_kern_entity_cap_drops_new_ids_but_still_merges_known() {
		let mut g = GraphGnn::new();
		let phantom = "remote-netB-k1";
		g.register(Kern::new(phantom, &g.root.id));
		{
			let kern = g.kerns.get_mut(phantom).unwrap();
			for i in 0..constants::GOSSIP_REMOTE_KERN_ENTITY_CAP {
				kern.entities.insert(format!("f{i}"), Entity::default());
			}
		}
		let changed = merge_remote_entity(
			&mut g,
			phantom,
			mk_entity("newid", "x", 1.0, EntityKind::Fact),
		);
		assert!(!changed, "new id past cap must be dropped");
		assert!(!g.kerns.get(phantom).unwrap().entities.contains_key("newid"));

		let mut known = mk_entity("f0", "x", 7.0, EntityKind::Fact);
		known.created_at = t(10);
		let changed = merge_remote_entity(&mut g, phantom, known);
		assert!(changed, "known id must still merge at cap");
		let f0 = g.kerns.get(phantom).unwrap().entities.get("f0").unwrap();
		assert_eq!(f0.created_at, t(10), "the join landed");
		assert_eq!(
			f0.heat, 0.0,
			"a remote re-assertion never raises heat, whatever it claims"
		);
	}

	#[test]
	fn remote_kern_insert_strips_every_attacker_settable_ranking_signal() {
		// SECURITY regression guard: first contact bypasses merge_entity's join
		// guards — the whole Entity would otherwise land verbatim.
		let mut g = GraphGnn::new();
		let phantom = "remote-netC-k1";
		g.register(Kern::new(phantom, &g.root.id));

		let mut poisoned = mk_entity("eP", "buy my product", 1.0e9, EntityKind::Fact);
		poisoned.conf_alpha = 1.0e9;
		poisoned.conf_beta = 0.0;
		poisoned.score = 1.0e9;
		poisoned.access_count.increment("attacker", u64::MAX);
		poisoned.accessed_at = t(1_000_000);
		poisoned.heat_updated_at = t(1_000_000);
		assert!(merge_remote_entity(&mut g, phantom, poisoned));

		let e = g.kerns.get(phantom).unwrap().entities.get("eP").unwrap();
		assert_eq!(e.heat, 0.0, "heat is earned by local access only");
		assert_eq!(e.heat_updated_at, None);
		assert_eq!(
			e.access_count.value(),
			0,
			"the qbst access term is neutered"
		);
		assert_eq!(e.accessed_at, None, "the qbst recency term is neutered");
		assert_eq!(e.conf_alpha, 0.0);
		assert_eq!(e.conf_beta, 0.0);
		assert_eq!(e.score, 0.5, "confidence resets to the neutral prior");
	}

	#[test]
	fn a_local_kern_merge_keeps_the_trusted_disk_absorb_path_intact() {
		// `absorb_graph` folds disk rows through the same entry point; only the
		// `remote-*` phantom kerns are untrusted.
		let mut g = GraphGnn::new();
		let local_kern = g.root.id.clone();
		let mut row = mk_entity("eD", "x", 3.0, EntityKind::Fact);
		row.access_count.increment("a", 4);
		assert!(merge_remote_entity(&mut g, &local_kern, row));

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
			"a locally cleared statement stays cleared across repeated gossip rounds"
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

		assert!(
			!merge_entity(&mut local, &remote),
			"lower lamport does not overwrite"
		);
		assert_eq!(
			local.valid_until,
			Some(UNIX_EPOCH + Duration::from_secs(100))
		);
	}
	#[test]
	fn a_remote_id_refused_at_the_cap_is_counted() {
		let mut g = GraphGnn::new();
		let phantom = "remote-netC-cap";
		g.register(Kern::new(phantom, &g.root.id));
		{
			let k = g.kerns.get_mut(phantom).expect("phantom kern");
			for i in 0..constants::GOSSIP_REMOTE_KERN_ENTITY_CAP {
				let id = format!("filler{i}");
				k.entities.insert(
					id.clone(),
					Entity {
						id,
						..Default::default()
					},
				);
			}
		}

		let before = remote_cap_dropped();
		let fresh = Entity {
			id: "brand-new".into(),
			..Default::default()
		};
		let merged = merge_remote_entity(&mut g, phantom, fresh);

		assert!(!merged, "a new id above the cap is refused");
		assert_eq!(
			remote_cap_dropped(),
			before + 1,
			"and the refusal is counted — a capped peer otherwise looks quiet"
		);
	}
}
