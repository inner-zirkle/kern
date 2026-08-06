use serde::{Deserialize, Serialize};

use crate::gossip_identity::PeerId;

/// Nearest ring neighbors kept per side. Greedy routing is correct as long
/// as `near` is correct, so these are maintained before any far link.
pub const RING_NEAR_K: usize = 4;
/// Target number of long links, sampled with density ~ 1/d (Kleinberg
/// exponent 1 for a 1-D ring) — what makes greedy routing O(log² n).
pub const RING_FAR_TARGET: usize = 8;
/// Hop budget for the iterative greedy join walk — comfortably above the
/// O(log² n) expectation for any plausible network size.
pub const RING_JOIN_MAX_HOPS: usize = 64;
/// A ring entry silent this long is evicted on the next heartbeat sweep.
pub const RING_ENTRY_TTL_SECS: u64 = 180;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerEntry {
	pub id: PeerId,
	pub addr: String,
	pub loc: f64,
	pub last_seen: u64,
}

/// Ring distance on the unit circle: the shorter way around.
pub fn ring_distance(a: f64, b: f64) -> f64 {
	let d = (a - b).abs();
	d.min(1.0 - d)
}

/// The circular view: k nearest neighbors each side plus ~1/d-sampled long
/// links. Replaces the flat peer list for WAN routing; LAN discovery stays a
/// bootstrap source that feeds `observe`.
pub struct RingView {
	loc: f64,
	near: Vec<PeerEntry>,
	far: Vec<PeerEntry>,
	// xorshift state for harmonic far-link sampling; seeded so behaviour is
	// reproducible in the sim tests.
	rng: u64,
}

impl RingView {
	pub fn new(loc: f64, seed: u64) -> Self {
		Self {
			loc,
			near: Vec::new(),
			far: Vec::new(),
			// xorshift needs a nonzero state.
			rng: seed.max(1),
		}
	}

	pub fn loc(&self) -> f64 {
		self.loc
	}

	pub fn near(&self) -> &[PeerEntry] {
		&self.near
	}

	pub fn far(&self) -> &[PeerEntry] {
		&self.far
	}

	pub fn len(&self) -> usize {
		self.near.len() + self.far.len()
	}

	pub fn is_empty(&self) -> bool {
		self.near.is_empty() && self.far.is_empty()
	}

	fn next_rand(&mut self) -> f64 {
		// xorshift64*
		let mut x = self.rng;
		x ^= x >> 12;
		x ^= x << 25;
		x ^= x >> 27;
		self.rng = x;
		(x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
	}

	// Clockwise arc from self to `loc`; `[0, 1)`. Side membership for the
	// near set: cw <= 0.5 is the clockwise side, the rest counter-clockwise.
	fn clockwise(&self, loc: f64) -> f64 {
		(loc - self.loc).rem_euclid(1.0)
	}

	/// Feed a peer sighting into the view. An existing entry (by id) is
	/// refreshed in place; a new one competes for a near slot first (near
	/// correctness beats everything), then for a far slot under harmonic
	/// (1/d) sampling. Self-sightings are ignored.
	pub fn observe(&mut self, entry: PeerEntry) {
		if entry.id == [0u8; 32] || ring_distance(entry.loc, self.loc) == 0.0 && entry.addr.is_empty()
		{
			return;
		}
		for e in self.near.iter_mut().chain(self.far.iter_mut()) {
			if e.id == entry.id {
				e.addr = entry.addr;
				e.loc = entry.loc;
				e.last_seen = e.last_seen.max(entry.last_seen);
				return;
			}
		}

		// Near admission: keep the RING_NEAR_K nearest on each side.
		let cw_side = self.clockwise(entry.loc) <= 0.5;
		let side_count = |v: &[PeerEntry], view: &Self| {
			v.iter()
				.filter(|e| (view.clockwise(e.loc) <= 0.5) == cw_side)
				.count()
		};
		let my_dist = ring_distance(entry.loc, self.loc);
		if side_count(&self.near, self) < RING_NEAR_K {
			self.near.push(entry);
			self.sort_near();
			return;
		}
		// Full side: displace the farthest same-side entry if we are closer.
		let farthest = self
			.near
			.iter()
			.enumerate()
			.filter(|(_, e)| (self.clockwise(e.loc) <= 0.5) == cw_side)
			.max_by(|(_, a), (_, b)| {
				ring_distance(a.loc, self.loc)
					.partial_cmp(&ring_distance(b.loc, self.loc))
					.unwrap_or(std::cmp::Ordering::Equal)
			})
			.map(|(i, e)| (i, ring_distance(e.loc, self.loc)));
		if let Some((i, worst)) = farthest {
			if my_dist < worst {
				let displaced = std::mem::replace(&mut self.near[i], entry);
				self.sort_near();
				// The displaced neighbor is still a fine far candidate.
				self.offer_far(displaced);
				return;
			}
		}
		self.offer_far(entry);
	}

	// Harmonic far admission: an empty slot is free; a full set replaces a
	// uniformly-chosen victim with probability w_new / (w_new + w_old) where
	// w = 1/d — a biased coin whose stationary bias follows Kleinberg's 1/d.
	fn offer_far(&mut self, entry: PeerEntry) {
		let d_new = ring_distance(entry.loc, self.loc).max(1e-9);
		if self.far.len() < RING_FAR_TARGET {
			self.far.push(entry);
			return;
		}
		let i = (self.next_rand() * self.far.len() as f64) as usize % self.far.len();
		let d_old = ring_distance(self.far[i].loc, self.loc).max(1e-9);
		let w_new = 1.0 / d_new;
		let w_old = 1.0 / d_old;
		if self.next_rand() < w_new / (w_new + w_old) {
			self.far[i] = entry;
		}
	}

	fn sort_near(&mut self) {
		let loc = self.loc;
		self.near.sort_by(|a, b| {
			ring_distance(a.loc, loc)
				.partial_cmp(&ring_distance(b.loc, loc))
				.unwrap_or(std::cmp::Ordering::Equal)
		});
	}

	/// Greedy route primitive: the known peer strictly closer to `target`
	/// than we are, or None — we are the terminal for this key.
	pub fn route(&self, target: f64) -> Option<&PeerEntry> {
		let own = ring_distance(self.loc, target);
		self.near
			.iter()
			.chain(self.far.iter())
			.min_by(|a, b| {
				ring_distance(a.loc, target)
					.partial_cmp(&ring_distance(b.loc, target))
					.unwrap_or(std::cmp::Ordering::Equal)
			})
			.filter(|e| ring_distance(e.loc, target) < own)
	}

	/// Drop entries not seen for `ttl_secs`. Near repairs itself from the far
	/// set: correctness of greedy routing depends only on near being right.
	pub fn evict_stale(&mut self, now_secs: u64, ttl_secs: u64) {
		let keep = |e: &PeerEntry| e.last_seen.saturating_add(ttl_secs) >= now_secs;
		self.near.retain(keep);
		self.far.retain(keep);
		// Promote far entries into empty near-side slots.
		let candidates: Vec<PeerEntry> = self.far.drain(..).collect();
		for c in candidates {
			self.observe(c);
		}
	}

	/// Remove a peer that hung up or was reported dead.
	pub fn remove(&mut self, id: &PeerId) {
		self.near.retain(|e| &e.id != id);
		self.far.retain(|e| &e.id != id);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn xs(state: &mut u64) -> u64 {
		let mut x = *state;
		x ^= x >> 12;
		x ^= x << 25;
		x ^= x >> 27;
		*state = x;
		x.wrapping_mul(0x2545_F491_4F6C_DD1D)
	}

	fn id_of(i: u64) -> PeerId {
		let mut id = [0u8; 32];
		id[..8].copy_from_slice(&i.to_le_bytes());
		id[8] = 1; // never the all-zero id `observe` ignores
		id
	}

	fn entry(i: u64, loc: f64) -> PeerEntry {
		PeerEntry {
			id: id_of(i),
			addr: format!("peer-{i}"),
			loc,
			last_seen: 1_000,
		}
	}

	#[test]
	fn ring_distance_takes_the_shorter_way_around() {
		assert_eq!(ring_distance(0.1, 0.2), ring_distance(0.2, 0.1));
		assert!((ring_distance(0.95, 0.05) - 0.1).abs() < 1e-12, "wraps");
		assert_eq!(ring_distance(0.3, 0.3), 0.0);
		assert!((ring_distance(0.0, 0.5) - 0.5).abs() < 1e-12, "antipode");
	}

	#[test]
	fn near_keeps_the_k_nearest_per_side_and_demotes_to_far() {
		let mut v = RingView::new(0.5, 7);
		// Six clockwise-side peers at increasing distance; k=4 stay near.
		for (i, loc) in [0.51, 0.52, 0.53, 0.54, 0.55, 0.56].iter().enumerate() {
			v.observe(entry(i as u64, *loc));
		}
		assert_eq!(v.near().len(), 4, "one side holds at most RING_NEAR_K");
		let worst_near = v
			.near()
			.iter()
			.map(|e| ring_distance(e.loc, 0.5))
			.fold(0.0, f64::max);
		assert!(
			(worst_near - 0.04).abs() < 1e-9,
			"the four nearest clockwise peers survive; got worst {worst_near}"
		);
		assert_eq!(v.far().len(), 2, "the displaced pair becomes far links");
	}

	#[test]
	fn observing_a_known_peer_refreshes_instead_of_duplicating() {
		let mut v = RingView::new(0.0, 7);
		v.observe(entry(1, 0.1));
		v.observe(PeerEntry {
			addr: "moved:1".into(),
			last_seen: 2_000,
			..entry(1, 0.1)
		});
		assert_eq!(v.len(), 1);
		assert_eq!(v.near()[0].addr, "moved:1");
		assert_eq!(v.near()[0].last_seen, 2_000);
	}

	#[test]
	fn route_returns_only_a_strictly_closer_peer() {
		let mut v = RingView::new(0.0, 7);
		v.observe(entry(1, 0.25));
		v.observe(entry(2, 0.5));
		let hop = v.route(0.4).expect("someone is closer to 0.4 than we are");
		assert_eq!(hop.loc, 0.5, "0.5 is 0.1 away; 0.25 is 0.15; we are 0.4");
		assert!(
			v.route(0.01).is_none(),
			"nobody beats us for a key at our own doorstep — we are terminal"
		);
	}

	#[test]
	fn evict_stale_drops_the_silent_and_promotes_far_into_near() {
		let mut v = RingView::new(0.0, 7);
		for (i, loc) in [0.01, 0.02, 0.03, 0.04].iter().enumerate() {
			v.observe(entry(i as u64, *loc));
		}
		// A fifth same-side peer lands in far.
		v.observe(entry(9, 0.05));
		assert_eq!(v.far().len(), 1);
		// Everyone near goes silent; the far peer stays fresh.
		for e in v.near.iter_mut() {
			e.last_seen = 0;
		}
		v.far[0].last_seen = 1_000;
		v.evict_stale(1_000, 500);
		assert_eq!(v.near().len(), 1, "the fresh far peer repaired near");
		assert_eq!(v.near()[0].loc, 0.05);
		assert!(v.far().is_empty());
	}

	// The Kleinberg gate: 1k synthetic peers, greedy routing reaches the peer
	// nearest to a random target in <= O(log² n) hops for 99% of targets.
	#[test]
	fn greedy_routing_reaches_the_nearest_peer_in_log_squared_hops() {
		let n = 1_000usize;
		let mut seed = 0xfeb5_1234_5678_9abcu64;
		let mut locs: Vec<f64> = (0..n)
			.map(|_| (xs(&mut seed) >> 11) as f64 / (1u64 << 53) as f64)
			.collect();
		locs.sort_by(|a, b| a.partial_cmp(b).unwrap());

		// Each peer learns its true ring neighbors (near) plus 32 random
		// sightings (far candidates) — the shape join + heartbeat converge to.
		let mut views: Vec<RingView> = Vec::with_capacity(n);
		for i in 0..n {
			let mut v = RingView::new(locs[i], 1 + i as u64);
			for off in 1..=RING_NEAR_K {
				let cw = (i + off) % n;
				let ccw = (i + n - off) % n;
				v.observe(entry(cw as u64, locs[cw]));
				v.observe(entry(ccw as u64, locs[ccw]));
			}
			for _ in 0..32 {
				let j = (xs(&mut seed) as usize) % n;
				if j != i {
					v.observe(entry(j as u64, locs[j]));
				}
			}
			views.push(v);
		}

		let peer_index = |addr: &str| -> usize { addr.strip_prefix("peer-").unwrap().parse().unwrap() };
		let max_hops = {
			let lg = (n as f64).log2();
			(lg * lg).ceil() as usize // ~100 for n=1000
		};

		let trials = 1_000;
		let mut reached = 0;
		for _ in 0..trials {
			let target = (xs(&mut seed) >> 11) as f64 / (1u64 << 53) as f64;
			let mut at = (xs(&mut seed) as usize) % n;
			let mut hops = 0;
			while hops < max_hops {
				match views[at].route(target) {
					Some(next) => {
						at = peer_index(&next.addr);
						hops += 1;
					}
					None => break,
				}
			}
			// Terminal peer must be the globally nearest to the target.
			let best = locs
				.iter()
				.map(|l| ring_distance(*l, target))
				.fold(f64::INFINITY, f64::min);
			if (ring_distance(locs[at], target) - best).abs() < 1e-12 {
				reached += 1;
			}
		}
		assert!(
			reached as f64 >= 0.99 * trials as f64,
			"greedy reached the nearest peer for only {reached}/{trials} targets"
		);
	}

	// Churn gate: kill 20% of peers, repair near from what heartbeat exchange
	// would re-observe, and routing still terminates at the nearest survivor.
	#[test]
	fn routing_survives_twenty_percent_churn_after_near_repairs() {
		let n = 500usize;
		let mut seed = 0xdead_beef_cafe_f00du64;
		let mut locs: Vec<f64> = (0..n)
			.map(|_| (xs(&mut seed) >> 11) as f64 / (1u64 << 53) as f64)
			.collect();
		locs.sort_by(|a, b| a.partial_cmp(b).unwrap());

		let mut alive = vec![true; n];
		let mut killed = 0;
		while killed < n / 5 {
			let j = (xs(&mut seed) as usize) % n;
			if alive[j] {
				alive[j] = false;
				killed += 1;
			}
		}
		let survivors: Vec<usize> = (0..n).filter(|i| alive[*i]).collect();

		// Survivors rebuild views over the surviving population only — the
		// eviction + peer-exchange repair loop, fast-forwarded.
		let mut views: std::collections::HashMap<usize, RingView> = std::collections::HashMap::new();
		for (rank, &i) in survivors.iter().enumerate() {
			let m = survivors.len();
			let mut v = RingView::new(locs[i], 1 + i as u64);
			for off in 1..=RING_NEAR_K {
				let cw = survivors[(rank + off) % m];
				let ccw = survivors[(rank + m - off) % m];
				v.observe(entry(cw as u64, locs[cw]));
				v.observe(entry(ccw as u64, locs[ccw]));
			}
			for _ in 0..16 {
				let j = survivors[(xs(&mut seed) as usize) % m];
				if j != i {
					v.observe(entry(j as u64, locs[j]));
				}
			}
			views.insert(i, v);
		}

		let peer_index = |addr: &str| -> usize { addr.strip_prefix("peer-").unwrap().parse().unwrap() };
		let lg = (n as f64).log2();
		let max_hops = (lg * lg).ceil() as usize;

		let trials = 400;
		let mut reached = 0;
		for _ in 0..trials {
			let target = (xs(&mut seed) >> 11) as f64 / (1u64 << 53) as f64;
			let mut at = survivors[(xs(&mut seed) as usize) % survivors.len()];
			let mut hops = 0;
			while hops < max_hops {
				match views[&at].route(target) {
					Some(next) => {
						let next = peer_index(&next.addr);
						assert!(alive[next], "repaired views never route to the dead");
						at = next;
						hops += 1;
					}
					None => break,
				}
			}
			assert!(hops < max_hops, "routing must terminate, not orbit");
			let best = survivors
				.iter()
				.map(|&i| ring_distance(locs[i], target))
				.fold(f64::INFINITY, f64::min);
			if (ring_distance(locs[at], target) - best).abs() < 1e-12 {
				reached += 1;
			}
		}
		assert!(
			reached as f64 >= 0.95 * trials as f64,
			"post-churn greedy reached the nearest survivor for only {reached}/{trials}"
		);
	}
}
