//! Convergent merge of one graph's rows into another's. The store is local and
//! single-writer, but a daemon can still meet state it did not write: an
//! external commit landing under a refused stale flush. Those rows join by the
//! same CRDT rules (`crdt`) rather than clobbering, so reconcile converges
//! whichever side wrote last.

use std::time::SystemTime;

use crate::graph::GraphGnn;
use base::base_types::{Entity, EntityStatus, Reason};
use base::crdt::lww_wins;

fn join_time(
	local: &mut Option<SystemTime>,
	incoming: Option<SystemTime>,
	take: impl Fn(SystemTime, SystemTime) -> bool,
) -> bool {
	match (*local, incoming) {
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

fn join_max_time(local: &mut Option<SystemTime>, incoming: Option<SystemTime>) -> bool {
	join_time(local, incoming, |r, l| r > l)
}

fn join_min_time(local: &mut Option<SystemTime>, incoming: Option<SystemTime>) -> bool {
	join_time(local, incoming, |r, l| r < l)
}

fn join_lww_time(
	local: &mut Option<SystemTime>,
	local_lamport: &mut u64,
	local_producer: &mut String,
	incoming: Option<SystemTime>,
	incoming_lamport: u64,
	incoming_producer: &str,
) -> bool {
	if lww_wins(
		(incoming_lamport, incoming_producer),
		(*local_lamport, local_producer.as_str()),
	) {
		*local = incoming;
		*local_lamport = incoming_lamport;
		*local_producer = incoming_producer.to_string();
		true
	} else {
		false
	}
}

fn join_superseded_by(local: &mut String, incoming: &str) -> bool {
	if !incoming.is_empty() && incoming > local.as_str() {
		*local = incoming.to_string();
		true
	} else {
		false
	}
}

pub fn merge_entity(local: &mut Entity, incoming: &Entity) -> bool {
	let mut changed = local.access_count.merge(&incoming.access_count);
	if incoming.heat > local.heat {
		local.heat = incoming.heat;
		changed = true;
	}
	// conf_alpha/conf_beta/unlinked_count are never imported from the incoming
	// side — a max-join on confidence is an irreversible pin, and evidence is
	// counted where it was observed.
	if incoming.status == EntityStatus::Superseded && local.status != EntityStatus::Superseded {
		local.status = EntityStatus::Superseded;
		changed = true;
	}
	changed |= join_superseded_by(&mut local.superseded_by, &incoming.superseded_by);
	changed |= join_min_time(&mut local.created_at, incoming.created_at);
	changed |= join_max_time(&mut local.accessed_at, incoming.accessed_at);
	changed |= join_max_time(&mut local.updated_at, incoming.updated_at);
	changed |= join_max_time(&mut local.heat_updated_at, incoming.heat_updated_at);
	changed |= join_lww_time(
		&mut local.valid_until,
		&mut local.valid_until_lamport,
		&mut local.valid_until_producer,
		incoming.valid_until,
		incoming.valid_until_lamport,
		&incoming.valid_until_producer,
	);
	// Statements are never imported. `id == content_hash(text)` and
	// `statements == [text]`, so a same-id row has identical content by
	// construction and a differing one asserts content its id does not hash to.
	// Unioning it both breaks content-addressing and resurrects a cleared statement.
	if changed {
		local.refresh_score();
	}
	changed
}

pub fn merge_reason(local: &mut Reason, incoming: &Reason) -> bool {
	let mut changed = local.traversal_count.merge(&incoming.traversal_count);
	if lww_wins(
		(incoming.score_lamport, &incoming.score_producer),
		(local.score_lamport, &local.score_producer),
	) {
		local.score = incoming.score;
		local.score_lamport = incoming.score_lamport;
		local.score_producer = incoming.score_producer.clone();
		changed = true;
	}
	changed
}

// An id owned by a DIFFERENT kern is rejected: entities are content-addressed
// and kern-owned, so the same id surfacing under two kerns is a reconcile bug,
// not a move. Owned by none → insert; already in target → CRDT-merge.
pub fn merge_entity_into(g: &mut GraphGnn, target_kern_id: &str, incoming: Entity) -> bool {
	let changed = merge_entity_into_inner(g, target_kern_id, incoming);
	if changed {
		// Merges mutate through `kerns` directly, never `get_mut` — the epoch
		// must still move or the daemon's query cache serves pre-merge results.
		g.bump_mutation_epoch();
	}
	changed
}

fn merge_entity_into_inner(g: &mut GraphGnn, target_kern_id: &str, incoming: Entity) -> bool {
	let host = g
		.kerns
		.iter()
		.find(|(_, k)| k.entities.contains_key(&incoming.id))
		.map(|(kid, _)| kid.clone());
	match host {
		Some(kid) if kid == target_kern_id => {
			let (changed, now_superseded) = match g.kerns.get_mut(&kid) {
				Some(kern) => match kern.entities.get_mut(&incoming.id) {
					Some(local) => {
						let changed = merge_entity(local, &incoming);
						(changed, local.status == EntityStatus::Superseded)
					}
					None => (false, false),
				},
				None => (false, false),
			};
			// A join that flipped to Superseded must evict from the ANN indices —
			// same invariant as `accept::supersede`: superseded is never a valid result.
			if now_superseded {
				g.entity_idx.delete(&incoming.id);
				g.gnn_entity_idx.delete(&incoming.id);
			}
			changed
		}
		Some(other) => {
			tracing::warn!(
				target: "kern.merge",
				id = %util::short_id(&incoming.id),
				owner = %other,
				target = %target_kern_id,
				"incoming entity id collides with an entity owned by another kern; rejected"
			);
			false
		}
		None => {
			let Some(kern) = g.kerns.get_mut(target_kern_id) else {
				tracing::warn!(target: "kern.merge", kern = %target_kern_id, "merge_entity_into: target kern missing; entity dropped");
				return false;
			};
			let id = incoming.id.clone();
			// Index on insert (mirrors `accept::commit_entity`) or the entity is
			// invisible to vector search until a rebuild; Superseded is stored, not indexed.
			let searchable = incoming.status != EntityStatus::Superseded;
			let vector = searchable
				.then(|| incoming.vector.clone())
				.filter(|v| !v.is_empty());
			let gnn_vector = searchable
				.then(|| incoming.gnn_vector.clone())
				.filter(|v| !v.is_empty());
			kern.entities.insert(id.clone(), incoming);
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
// same CRDT joins, and the caller retries the flush with the disk
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
			if merge_entity_into(local, &kid, e) {
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
					crate::reason::add_reason(lkern, r);
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
#[path = "tests/merge_test.rs"]
mod merge_tests;
