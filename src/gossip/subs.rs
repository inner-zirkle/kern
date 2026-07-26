use std::collections::HashMap;

use parking_lot::RwLock;

use super::contract::ContractId;

/// Subscription table entries are cheap; the bound exists so a hostile peer
/// cannot grow the table without limit (LRU on last touch).
pub const SUB_TABLE_CAP: usize = 256;

#[derive(Debug, Clone, Default)]
pub struct SubEntry {
	/// Next hop toward `loc(ContractId)`; None = we are the tree root (or
	/// have not resolved a parent yet).
	pub upstream: Option<String>,
	/// Subscribers who reached us; deltas fan out to them.
	pub downstream: Vec<String>,
	pub last_touch: u64,
}

/// Per-contract tree links: subscribers form a tree along routing paths —
/// each hop records who asked, and who it asked in turn (FEDERATION_PLAN §4).
#[derive(Default)]
pub struct SubTable {
	inner: RwLock<HashMap<ContractId, SubEntry>>,
}

impl SubTable {
	pub fn new() -> Self {
		Self::default()
	}

	fn touch_locked(map: &mut HashMap<ContractId, SubEntry>, id: &ContractId) {
		if let Some(e) = map.get_mut(id) {
			e.last_touch = crate::base::util::now_secs();
		}
	}

	fn evict_over_cap(map: &mut HashMap<ContractId, SubEntry>) {
		while map.len() > SUB_TABLE_CAP {
			let oldest = map
				.iter()
				.min_by_key(|(_, e)| e.last_touch)
				.map(|(k, _)| *k);
			match oldest {
				Some(k) => {
					map.remove(&k);
				}
				None => break,
			}
		}
	}

	pub fn add_downstream(&self, id: &ContractId, addr: &str) {
		if addr.is_empty() {
			return;
		}
		let mut map = self.inner.write();
		let e = map.entry(*id).or_default();
		if !e.downstream.iter().any(|a| a == addr) {
			e.downstream.push(addr.to_string());
		}
		Self::touch_locked(&mut map, id);
		Self::evict_over_cap(&mut map);
	}

	pub fn set_upstream(&self, id: &ContractId, addr: Option<String>) {
		let mut map = self.inner.write();
		let e = map.entry(*id).or_default();
		e.upstream = addr;
		Self::touch_locked(&mut map, id);
		Self::evict_over_cap(&mut map);
	}

	pub fn upstream(&self, id: &ContractId) -> Option<String> {
		self.inner.read().get(id).and_then(|e| e.upstream.clone())
	}

	pub fn is_downstream(&self, id: &ContractId, addr: &str) -> bool {
		self
			.inner
			.read()
			.get(id)
			.map(|e| e.downstream.iter().any(|a| a == addr))
			.unwrap_or(false)
	}

	pub fn contracts(&self) -> Vec<ContractId> {
		self.inner.read().keys().copied().collect()
	}

	/// Everyone a changed delta forwards to: upstream plus all downstream,
	/// minus the peer it arrived from (natural flood suppression; the
	/// seen-set backstops cycles).
	pub fn fanout(&self, id: &ContractId, except: &str) -> Vec<String> {
		let map = self.inner.read();
		let Some(e) = map.get(id) else {
			return Vec::new();
		};
		let mut out: Vec<String> = Vec::new();
		if let Some(up) = &e.upstream {
			if up != except {
				out.push(up.clone());
			}
		}
		for d in &e.downstream {
			if d != except && !out.contains(d) {
				out.push(d.clone());
			}
		}
		out
	}

	pub fn remove(&self, id: &ContractId) {
		self.inner.write().remove(id);
	}

	pub fn len(&self) -> usize {
		self.inner.read().len()
	}

	pub fn is_empty(&self) -> bool {
		self.inner.read().is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn cid(n: u8) -> ContractId {
		[n; 32]
	}

	#[test]
	fn fanout_reaches_up_and_down_but_never_back_where_it_came_from() {
		let t = SubTable::new();
		t.set_upstream(&cid(1), Some("parent:1".into()));
		t.add_downstream(&cid(1), "child-a:1");
		t.add_downstream(&cid(1), "child-b:1");
		t.add_downstream(&cid(1), "child-a:1"); // dupe is idempotent

		let all = t.fanout(&cid(1), "");
		assert_eq!(all, vec!["parent:1", "child-a:1", "child-b:1"]);
		let from_parent = t.fanout(&cid(1), "parent:1");
		assert_eq!(from_parent, vec!["child-a:1", "child-b:1"]);
		let from_child = t.fanout(&cid(1), "child-a:1");
		assert_eq!(from_child, vec!["parent:1", "child-b:1"]);
		assert!(
			t.fanout(&cid(2), "").is_empty(),
			"unknown contract, no fanout"
		);
	}

	#[test]
	fn the_table_is_bounded_and_evicts_the_least_recently_touched() {
		let t = SubTable::new();
		for i in 0..=SUB_TABLE_CAP {
			let mut id = [0u8; 32];
			id[..8].copy_from_slice(&(i as u64).to_le_bytes());
			id[31] = 1;
			t.add_downstream(&id, "peer:1");
		}
		assert!(
			t.len() <= SUB_TABLE_CAP,
			"a hostile subscriber cannot grow the table without bound"
		);
	}
}
