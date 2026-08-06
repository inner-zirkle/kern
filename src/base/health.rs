use crate::base::graph::GraphGnn;

// `Default` is what lets a caller name the one or two counters it cares about
// without reading the process statics `graph_health_stats` reads — which is the
// difference between a test that is runner-independent and one that reds the
// moment another test in the same process increments a counter.
// `f64` is not `Eq`, so the lower-confidence-bound field below drops the derive.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HealthStats {
	pub kerns: usize,
	pub entities: usize,
	pub reasons: usize,
	pub unnamed: usize,
	pub gravitons: Vec<String>,
	// Cold rows dropped by the FIFO cap since this process opened the store. A
	// dropped non-durable entity is unrecoverable, so the count is its only trace.
	pub cold_evicted: u64,
	pub embed_model: String,
	pub embed_dim: usize,
	pub embed_mismatch: bool,
	// Queries dropped by the dimension guard since this process opened. Nonzero
	// with embed_mismatch false means something upstream embeds off-model.
	pub query_dim_rejected: u64,
	// Deliveries that bypassed `min_deliver_score` because nothing cleared it —
	// a degraded answer the caller cannot distinguish from a confident one.
	pub below_floor_deliveries: u64,
	// Entities GC could not age because their timestamp is in the future.
	// Nonzero means compaction is stalled on a clock problem, not on policy.
	pub clock_skew_skips: u64,
	// Chunks dropped because embedding them failed — an empty graph caused by a
	// dead endpoint rather than by an empty corpus.
	pub ingest_dropped_chunks: u64,
	// New remote ids refused because their phantom kern is at the entity cap.
	pub remote_cap_dropped: u64,
	// Entities dropped with no cold store bound. Spill-before-drop does not hold
	// for an in-memory kern; this is how far that deployment has diverged from a
	// durable one.
	pub unspilled_drops: u64,
	// Jobs the ingest queue refused because it was full. Nonzero means a producer
	// is outrunning the LLM leg and text was handed back, not stored.
	pub ingest_queue_refused: u64,
	// Gini coefficient over resident entities' access counts — 0.0 when every
	// entity is accessed equally (converged on uniform paths), →1.0 when one
	// entity holds all access (not converged). Empty graph → 0.0. Makes the
	// "corpus converges on efficient paths" claim measurable (ROADMAP item 62).
	pub gini_access: f64,
	// The resident-kern cap (`KERN_CAP_DISABLED` = uncapped). Armed via
	// `apply_graph_config`; surfaced so an operator sees the bound and a warn
	// when resident kerns approach it (ROADMAP item 83).
	pub max_kerns: usize,
	// Supersede chains that exceeded `SUPERSEDE_CHAIN_HOP_THRESHOLD` on one
	// `external_id` (ROADMAP item 58 trigger #1). Process-global; the count is
	// the only trace a contested chain ran past the hop budget.
	pub supersede_chain_depth_exceeded: u64,
	// The largest resident kern's entity count — a gauge of the unbounded
	// resident set at the per-kern granularity the kern cap cannot see (a cap
	// on kerns bounds the count of kerns, not the size of any one). Empty graph
	// → 0. Measures, does not enforce (ROADMAP item 83).
	pub largest_kern_entities: usize,
	// Gini coefficient over resident kern sizes (`entities.len()`) — the
	// distribution the `largest_kern_entities` max only summarises. 0.0 when all
	// kerns hold the same count (balanced), →1.0 asymptotically as one kern
	// holds all entities (finite-n max (n−1)/n). Empty graph → 0.0. Measures,
	// does not enforce (ROADMAP item 83).
	pub gini_kern_sizes: f64,
}

/// Gini coefficient over the access-count distribution. 0.0 when all counts are
/// equal (or empty); →1.0 when one entity holds all access. Standard formula
/// `G = (Σ_i Σ_j |x_i − x_j|) / (2 n Σ x)`; 0.0 for an empty or zero-sum slice.
pub fn gini_over_access(counts: &[u64]) -> f64 {
	let n = counts.len();
	if n == 0 {
		return 0.0;
	}
	let sum: u64 = counts.iter().sum();
	if sum == 0 {
		return 0.0;
	}
	let mut abs_diff_sum: u128 = 0;
	for i in 0..n {
		for j in 0..n {
			abs_diff_sum += (counts[i].max(counts[j]) - counts[i].min(counts[j])) as u128;
		}
	}
	let denom = 2u128 * (n as u128) * (sum as u128);
	(abs_diff_sum as f64) / (denom as f64)
}

/// Gini coefficient over the kern-size distribution (`entities.len()` per
/// resident kern). Same formula as `gini_over_access`, cast to `u64`; 0.0 when
/// all kerns hold the same count (or empty), →1.0 asymptotically (finite-n max
/// `(n−1)/n`). Measures the balance the `largest_kern_entities` max summarises
/// (ROADMAP item 83).
pub fn gini_over_kern_sizes(counts: &[usize]) -> f64 {
	let n = counts.len();
	if n == 0 {
		return 0.0;
	}
	let sum: u64 = counts.iter().map(|c| *c as u64).sum();
	if sum == 0 {
		return 0.0;
	}
	let mut abs_diff_sum: u128 = 0;
	for i in 0..n {
		for j in 0..n {
			abs_diff_sum += (counts[i].max(counts[j]) - counts[i].min(counts[j])) as u128;
		}
	}
	let denom = 2u128 * (n as u128) * (sum as u128);
	(abs_diff_sum as f64) / (denom as f64)
}

pub fn graph_health_stats(g: &GraphGnn) -> HealthStats {
	let kerns = g.all();
	let mut entities = 0usize;
	let mut reasons = 0usize;
	let mut unnamed = 0usize;
	for k in &kerns {
		entities += k.entities.len();
		reasons += k.reasons.len();
		if k.is_unnamed() {
			unnamed += 1;
		}
	}
	let gravitons: Vec<String> = crate::base::accept::root_graviton_ids(g)
		.iter()
		.filter_map(|cid| g.loaded(cid))
		.map(|c| c.graviton_text.clone())
		.collect();
	// Resident entities only — an unloaded kern's access counts are on disk and
	// not part of the live distribution the metric describes.
	let access_counts: Vec<u64> = g
		.all()
		.iter()
		.flat_map(|k| k.entities.values().map(|e| e.access_count.value()))
		.collect();
	let gini_access = gini_over_access(&access_counts);
	// Largest resident kern's entity count — the per-kern size the kern cap
	// does not bound. Reuses the `kerns` walk already done above; computed here
	// for one pass over the resident map.
	let largest_kern_entities = kerns.iter().map(|k| k.entities.len()).max().unwrap_or(0);
	// Gini over kern sizes — the distribution the max summarises. Same walk.
	let kern_sizes: Vec<usize> = kerns.iter().map(|k| k.entities.len()).collect();
	let gini_kern_sizes = gini_over_kern_sizes(&kern_sizes);
	let store = g.store();
	let stamp = store
		.as_ref()
		.and_then(|s| s.embed_stamp())
		.unwrap_or_default();
	HealthStats {
		kerns: kerns.len(),
		entities,
		reasons,
		unnamed,
		gravitons,
		cold_evicted: store.as_ref().map(|s| s.cold_evicted()).unwrap_or(0),
		embed_model: stamp.model,
		// An unstamped store still knows what it indexed.
		embed_dim: match stamp.dim {
			0 => g.entity_vector_dim().unwrap_or(0),
			d => d,
		},
		embed_mismatch: store.map(|s| s.embed_mismatch()).unwrap_or(false),
		query_dim_rejected: crate::base::search::query_dim_rejected(),
		below_floor_deliveries: crate::retrieval::score::below_floor_deliveries(),
		clock_skew_skips: crate::tick_stigmergy::clock_skew_skips(),
		ingest_dropped_chunks: crate::ingest::worker::ingest_dropped_chunks(),
		remote_cap_dropped: crate::base::merge::remote_cap_dropped(),
		unspilled_drops: crate::tick_stigmergy::unspilled_drops(),
		ingest_queue_refused: crate::ingest::worker::ingest_queue_refused(),
		gini_access,
		max_kerns: g.max_loaded_kerns(),
		supersede_chain_depth_exceeded: crate::base::accept::supersede_chain_depth_exceeded(),
		largest_kern_entities,
		gini_kern_sizes,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::base::store::{EmbedStamp, Store};

	#[test]
	fn empty_graph_reports_no_entities_or_reasons() {
		let g = GraphGnn::new();
		let h = graph_health_stats(&g);
		assert_eq!(h.entities, 0, "fresh graph has no entities");
		assert_eq!(h.reasons, 0, "fresh graph has no reasons");
		assert!(h.kerns >= 1, "at least the root kern is present");
		assert!(h.gravitons.len() <= h.kerns);
	}

	#[test]
	fn storeless_graph_reports_zeroed_store_signals() {
		let h = graph_health_stats(&GraphGnn::new());
		assert_eq!(h.cold_evicted, 0);
		assert!(h.embed_model.is_empty());
		assert_eq!(h.embed_dim, 0);
		assert!(!h.embed_mismatch);
	}

	#[test]
	fn store_signals_surface_evictions_and_the_embed_stamp() {
		use crate::base::types::{mk_entity, EntityKind};

		let d = tempfile::tempdir().unwrap();
		let store = Store::open(&d.path().to_string_lossy()).unwrap();
		for i in 0..3 {
			let mut e = mk_entity(&format!("e{i}"), "cold", 0.0, EntityKind::Claim);
			e.created_at =
				Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100 * (i as u64 + 1)));
			store.cold_spill(&e).unwrap();
		}
		store
			.check_embed_stamp(&EmbedStamp {
				model: "qwen3".into(),
				dim: 1024,
			})
			.unwrap();
		store
			.check_embed_stamp(&EmbedStamp {
				model: "nomic".into(),
				dim: 768,
			})
			.unwrap();

		let mut g = GraphGnn::new();
		g.set_store(std::sync::Arc::new(store));
		let h = graph_health_stats(&g);
		assert_eq!(h.embed_model, "qwen3", "health names the STORED model");
		assert_eq!(h.embed_dim, 1024);
		assert!(h.embed_mismatch, "the model swap is visible to an operator");
		assert_eq!(h.cold_evicted, 0, "under the cap nothing is evicted");

		g.store().unwrap().cold_cap(1).unwrap();
		assert_eq!(
			graph_health_stats(&g).cold_evicted,
			2,
			"evicted rows are reported, not silently dropped"
		);
	}

	#[test]
	fn gini_over_access_pins_known_distributions() {
		assert_eq!(gini_over_access(&[]), 0.0, "empty -> 0.0");
		assert_eq!(gini_over_access(&[5, 5, 5]), 0.0, "uniform -> 0.0");
		assert_eq!(gini_over_access(&[1, 1, 1, 1]), 0.0, "uniform -> 0.0");
		assert_eq!(gini_over_access(&[0, 0, 0]), 0.0, "zero-sum -> 0.0");
		// [10,0,0]: standard Gini, one entity holds all access over n=3 ->
		// (n-1)/n = 2/3.
		assert!(
			(gini_over_access(&[10, 0, 0]) - 2.0 / 3.0).abs() < 1e-12,
			"one of three holds all access -> 2/3"
		);
		// [100,0]: n=2 -> (n-1)/n = 1/2.
		assert!(
			(gini_over_access(&[100, 0]) - 0.5).abs() < 1e-12,
			"two entities, one holds all -> 0.5"
		);
	}

	#[test]
	fn graph_health_stats_empty_graph_gini_is_zero() {
		let h = graph_health_stats(&GraphGnn::new());
		assert_eq!(h.gini_access, 0.0, "no entities -> uniform 0.0");
	}

	#[test]
	fn graph_health_stats_skewed_access_gini_above_half() {
		use crate::base::types::{mk_entity, EntityKind};
		use crate::crdt::GCounter;

		let mut g = GraphGnn::new();
		// One heavily-accessed entity, several untouched. Insert via
		// `root_kern_mut` so the kerns-map root (what `all()` reads) is mutated,
		// not the separate `g.root` field.
		{
			let root = g.root_kern_mut().expect("root kern present");
			let mut hot = mk_entity("hot", "hot", 0.0, EntityKind::Claim);
			hot.access_count = {
				let mut c = GCounter::new();
				c.increment("local", 50);
				c
			};
			root.entities.insert("hot".into(), hot);
			for i in 0..5 {
				let cold = mk_entity(&format!("c{i}"), "cold", 0.0, EntityKind::Claim);
				root.entities.insert(format!("c{i}"), cold);
			}
		}
		let h = graph_health_stats(&g);
		assert!(
			h.gini_access > 0.5,
			"one entity holds all access -> gini > 0.5, got {}",
			h.gini_access
		);
	}

	#[test]
	fn graph_health_stats_reports_max_kerns() {
		use crate::base::constants::KERN_CAP_DISABLED;

		// Default graph: uncapped.
		let h = graph_health_stats(&GraphGnn::new());
		assert_eq!(
			h.max_kerns, KERN_CAP_DISABLED,
			"fresh graph carries the disabled sentinel"
		);

		// An armed cap is surfaced.
		let mut g = GraphGnn::new();
		g.set_max_loaded_kerns(8);
		let h = graph_health_stats(&g);
		assert_eq!(h.max_kerns, 8, "armed cap reaches HealthStats");
	}

	#[test]
	fn graph_health_stats_reports_largest_kern_entities() {
		use crate::base::types::{mk_entity, EntityKind, Kern};

		// Empty graph -> 0.
		assert_eq!(
			graph_health_stats(&GraphGnn::new()).largest_kern_entities,
			0,
			"empty graph -> no resident entities"
		);

		// One kern holding 10 entities + four empty named children: the max is 10.
		let mut g = GraphGnn::new();
		{
			let root = g.root_kern_mut().expect("root kern present");
			for i in 0..10 {
				let e = mk_entity(&format!("e{i}"), "x", 0.0, EntityKind::Claim);
				root.entities.insert(format!("e{i}"), e);
			}
		}
		for i in 0..4 {
			let child = Kern::new_named_child(&g.root.id, &g.root.id, &format!("c{i}"), vec![0.0; 4]);
			g.kerns.insert(child.id.clone(), child);
		}
		let h = graph_health_stats(&g);
		assert_eq!(
			h.largest_kern_entities, 10,
			"largest resident kern holds 10 entities; the four empty children do not move the max"
		);
	}

	#[test]
	fn gini_over_kern_sizes_pins_known_distributions() {
		assert_eq!(gini_over_kern_sizes(&[]), 0.0, "empty -> 0.0");
		assert_eq!(gini_over_kern_sizes(&[5, 5, 5]), 0.0, "uniform -> 0.0");
		assert_eq!(gini_over_kern_sizes(&[1, 1, 1, 1]), 0.0, "uniform -> 0.0");
		assert_eq!(gini_over_kern_sizes(&[0, 0, 0]), 0.0, "zero-sum -> 0.0");
		// [10,0,0]: one kern holds all entities over n=3 -> (n-1)/n = 2/3.
		assert!(
			(gini_over_kern_sizes(&[10, 0, 0]) - 2.0 / 3.0).abs() < 1e-12,
			"one of three holds all -> 2/3"
		);
		// [100,0]: n=2 -> (n-1)/n = 1/2.
		assert!(
			(gini_over_kern_sizes(&[100, 0]) - 0.5).abs() < 1e-12,
			"two kerns, one holds all -> 0.5"
		);
	}

	#[test]
	fn graph_health_stats_reports_gini_kern_sizes() {
		use crate::base::types::{mk_entity, EntityKind, Kern};

		// Empty graph -> 0.0 (one root kern, no entities -> uniform zero-sum).
		assert!(
			graph_health_stats(&GraphGnn::new()).gini_kern_sizes.abs() < 1e-12,
			"empty graph -> gini 0.0"
		);

		// One kern holding 10 entities + four empty named children: the
		// distribution is [10,0,0,0,0] (root + 4 children), n=5, sum=10 ->
		// Gini = (n-1)/n = 4/5 = 0.8 (> 0.5).
		let mut g = GraphGnn::new();
		{
			let root = g.root_kern_mut().expect("root kern present");
			for i in 0..10 {
				let e = mk_entity(&format!("e{i}"), "x", 0.0, EntityKind::Claim);
				root.entities.insert(format!("e{i}"), e);
			}
		}
		for i in 0..4 {
			let child = Kern::new_named_child(&g.root.id, &g.root.id, &format!("c{i}"), vec![0.0; 4]);
			g.kerns.insert(child.id.clone(), child);
		}
		let h = graph_health_stats(&g);
		assert!(
			h.gini_kern_sizes > 0.5,
			"one kern holds all entities -> gini > 0.5, got {}",
			h.gini_kern_sizes
		);
	}

	#[test]
	fn graph_health_stats_carries_supersede_chain_depth_exceeded() {
		// The field is wired from the process-global read fn; on a fresh graph
		// (no supersede happened in this process before this point is reached)
		// it equals the read fn's current value. The increment itself is pinned
		// in `accept::tests::supersede_chain_depth_counter_increments_past_threshold`.
		let h = graph_health_stats(&GraphGnn::new());
		assert_eq!(
			h.supersede_chain_depth_exceeded,
			crate::base::accept::supersede_chain_depth_exceeded(),
			"HealthStats mirrors the process-global counter"
		);
	}
}
