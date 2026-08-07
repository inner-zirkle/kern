//! Seeding: the first candidate set for a query, fused from the content index,
//! the GNN-propagated index, and BM25 lexical hits per the mode's weights,
//! with importance fallbacks so an empty ANN result still seeds a walk.

use crate::config::RetrievalConfig;
use crate::graph::GraphGnn;
use crate::lexical::LexicalIndex;
use math::cosine;
use crate::retrieval::score::{matches_filter, QueryOptions};
use crate::search::{
	search_all_filtered, search_all_unlocked, search_reasons_all_unlocked, EntityHit,
};
use rayon::iter::Either;
use rayon::prelude::*;
use std::collections::HashMap;

// Below this an in-kern split costs more than the walk it splits; see `seed_important`.
const PARALLEL_SCAN_MIN_ENTITIES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
	Content,
	Reason,
	Hybrid,
}

impl Mode {
	pub fn parse(s: &str) -> Self {
		match s.to_lowercase().as_str() {
			"content" => Self::Content,
			"reason" => Self::Reason,
			_ => Self::Hybrid,
		}
	}
}

#[derive(Debug, Clone, Copy)]
pub struct Weights {
	pub content: f64,
	pub reason: f64,
	pub edge: f64,
}

impl Weights {
	pub fn for_mode(cfg: &RetrievalConfig, m: Mode) -> Self {
		let w = match m {
			Mode::Content => cfg.weights_content,
			Mode::Reason => cfg.weights_reason,
			Mode::Hybrid => cfg.weights_hybrid,
		};
		Self {
			content: w.content,
			reason: w.reason,
			edge: w.edge,
		}
	}
}

pub fn seed_with_important(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	query_vec: &[f32],
	k: usize,
	mode: Mode,
	opts: Option<&QueryOptions>,
	important: &[EntityHit],
) -> Vec<EntityHit> {
	let mut hits = match mode {
		Mode::Reason => seed_by_reason(g, query_vec, k),
		// Filter DURING the ANN traversal so a sparse filter still yields k matching hits (not an unfiltered top-k post-filtered to fewer).
		_ => match opts {
			Some(o) if o.is_active() => {
				let keep = matches_keep(g, o);
				search_all_filtered(g, query_vec, k, &keep)
			}
			_ => search_all_unlocked(g, query_vec, k),
		},
	};
	hits = merge_seeds(hits, important.to_vec());
	hits.truncate(k.max(cfg.seed_k));
	hits
}

// The single filter shared by dense ANN, lexical, and post-filter, so they never diverge.
fn matches_keep<'a>(g: &'a GraphGnn, opts: &'a QueryOptions) -> impl Fn(&str) -> bool + 'a {
	move |id: &str| {
		g.kern_of_entity(id)
			.and_then(|kid| g.kerns.get(kid))
			.and_then(|kern| kern.entities.get(id))
			.is_some_and(|e| matches_filter(e, opts))
	}
}

pub fn seed_lexical(
	lex: &LexicalIndex,
	g: &GraphGnn,
	query_text: &str,
	k: usize,
	opts: Option<&QueryOptions>,
) -> Vec<EntityHit> {
	// Filter BEFORE the BM25 top-k truncation, so a sparse filter still yields k matching lexical hits.
	let raw = match opts {
		Some(o) if o.is_active() => lex.search_filtered(query_text, k, &matches_keep(g, o)),
		_ => lex.search(query_text, k),
	};
	raw
		.into_iter()
		.map(|h| EntityHit {
			entity_id: h.entity_id,
			score: h.score as f64,
		})
		.collect()
}

fn seed_by_reason(g: &GraphGnn, query_vec: &[f32], k: usize) -> Vec<EntityHit> {
	let reason_hits = search_reasons_all_unlocked(g, query_vec, k);
	let mut seen = HashMap::new();
	for rh in &reason_hits {
		let reason = g
			.kern_of_reason(&rh.reason_id)
			.and_then(|kid| g.loaded(kid))
			.and_then(|kern| kern.reasons.get(&rh.reason_id));
		if let Some(r) = reason {
			let entry = seen.entry(r.from.clone()).or_insert(0.0_f64);
			if rh.score > *entry {
				*entry = rh.score;
			}
		}
	}
	let mut hits: Vec<EntityHit> = seen.into_iter().map(EntityHit::from).collect();
	hits.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
	hits
}

pub fn seed_important(
	g: &GraphGnn,
	cfg: &RetrievalConfig,
	query_vec: &[f32],
	opts: Option<&QueryOptions>,
) -> Vec<EntityHit> {
	let kerns = g.all();
	let min_cos = cfg.important_min_cosine;
	let access_threshold = cfg.important_access_threshold;
	// Importance must respect an active filter at the SOURCE: non-matching important entities would crowd the merged seed and truncate matching ones out before the post-filter.
	let active_filter = opts.filter(|o| o.is_active());
	let mut hits: Vec<EntityHit> = kerns
		.par_iter()
		.flat_map(|kern| {
			// SECURITY: a peer picks its own kind, so a remote Fact does not inherit the
			// Fact bypass of the access gate. Merge zeroes remote access_count, so this
			// leaves remote entities gated on access they can never accrue remotely —
			// they enter the seed pool only once LOCAL use earns it.
			let remote_kern = kern.is_remote();
			let gate = move |t: &base::base_types::Entity| -> Option<EntityHit> {
				if !t.has_vector() {
					return None;
				}
				if let Some(o) = active_filter {
					if !matches_filter(t, o) {
						return None;
					}
				}
				let privileged_kind = t.is_fact() && !remote_kern;
				let dominated = !privileged_kind && t.access_count.value_i32() < access_threshold;
				if dominated {
					return None;
				}
				let score = cosine(query_vec, &t.vector);
				(score >= min_cos).then(|| EntityHit {
					entity_id: t.id.clone(),
					score,
				})
			};
			// `flat_map_iter` over `g.all()` parallelises over KERNS only, so the ordinary
			// single-kern corpus walked the whole graph on one thread however many cores
			// were free. Splitting inside the kern costs a fixed ~0.2 ms of rayon setup,
			// which on a small corpus exceeds the entire scan — measured 0.22x at N=1k and
			// 0.43x at N=10k against 1.6-2.8x at N=100k. So the split is earned by size,
			// and everything under the threshold keeps exactly the walk it already had.
			if kern.entities.len() >= PARALLEL_SCAN_MIN_ENTITIES {
				Either::Left(kern.entities.par_iter().filter_map(move |(_, t)| gate(t)))
			} else {
				Either::Right(
					kern
						.entities
						.values()
						.filter_map(gate)
						.collect::<Vec<_>>()
						.into_par_iter(),
				)
			}
		})
		.collect();
	hits.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
	hits
}

pub fn merge_seeds(a: Vec<EntityHit>, b: Vec<EntityHit>) -> Vec<EntityHit> {
	let scored =
		math::softmax_merge_scores(a.into_iter().chain(b).map(|h| (h.entity_id, h.score)));
	let mut out: Vec<EntityHit> = scored.into_iter().map(EntityHit::from).collect();
	out.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use base::base_types::{Entity, EntityKind, Kern};

	fn ent(id: &str, vector: Vec<f32>, access: u64, fact: bool) -> Entity {
		let mut e = Entity {
			id: id.into(),
			vector: vector.into(),
			kind: if fact {
				EntityKind::Fact
			} else {
				EntityKind::Claim
			},
			..Default::default()
		};
		if access > 0 {
			e.access_count.increment("t", access);
		}
		e
	}

	fn graph_with(entities: Vec<Entity>) -> GraphGnn {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		for e in entities {
			k.entities.insert(e.id.clone(), e);
		}
		g.kerns.insert("kx".into(), k);
		g
	}

	fn xorshift(s: &mut u64) -> u64 {
		*s ^= *s << 13;
		*s ^= *s >> 7;
		*s ^= *s << 17;
		*s
	}

	// Three kerns (one remote), empty vectors, both kinds, access spread across the
	// gate — every branch of the predicate is populated. Stays under
	// PARALLEL_SCAN_MIN_ENTITIES on purpose: this covers the gate and the serial
	// walk, and `the_in_kern_parallel_split_selects_exactly_what_the_serial_walk_does`
	// covers the split.
	fn generated_graph(seed: u64, n: usize) -> GraphGnn {
		let mut s = seed
			.wrapping_mul(6364136223846793005)
			.wrapping_add(1442695040888963407)
			| 1;
		let mut g = GraphGnn::new();
		let names = ["kx", "ky", "remote-peer-k1"];
		let mut kerns: Vec<Kern> = names.iter().map(|id| Kern::new(*id, "")).collect();
		for i in 0..n {
			let vector = if i % 17 == 0 {
				Vec::new()
			} else {
				(0..8)
					.map(|_| (xorshift(&mut s) % 2000) as f32 / 1000.0 - 1.0)
					.collect()
			};
			let e = ent(
				&format!("e{i:05}"),
				vector,
				xorshift(&mut s) % 8,
				i % 4 == 0,
			);
			kerns[i % names.len()].entities.insert(e.id.clone(), e);
		}
		for k in kerns {
			g.kerns.insert(k.id.clone(), k);
		}
		g
	}

	// The gate, spelled out sequentially. Deliberately NOT sharing code with
	// seed_important: a reference that calls the thing under test proves nothing.
	fn reference_important(
		g: &GraphGnn,
		cfg: &RetrievalConfig,
		q: &[f32],
		opts: Option<&QueryOptions>,
	) -> Vec<(String, f64)> {
		let active = opts.filter(|o| o.is_active());
		let mut out: Vec<(String, f64)> = Vec::new();
		for kern in g.all() {
			let remote = kern.is_remote();
			for t in kern.entities.values() {
				if !t.has_vector() {
					continue;
				}
				if let Some(o) = active {
					if !matches_filter(t, o) {
						continue;
					}
				}
				let privileged = t.is_fact() && !remote;
				if !privileged && t.access_count.value_i32() < cfg.important_access_threshold {
					continue;
				}
				let score = cosine(q, &t.vector);
				if score >= cfg.important_min_cosine {
					out.push((t.id.clone(), score));
				}
			}
		}
		out.sort_by(|a, b| util::cmp_rank(a.1, &a.0, b.1, &b.0));
		out
	}

	fn cfg() -> RetrievalConfig {
		RetrievalConfig {
			important_min_cosine: 0.5,
			important_access_threshold: 5,
			..Default::default()
		}
	}

	#[test]
	fn a_remote_fact_does_not_bypass_the_importance_access_gate() {
		let mut g = graph_with(vec![ent("local_fact", vec![1.0, 0.0], 0, true)]);
		let mut phantom = Kern::new("remote-evilnet-k1", "");
		phantom.entities.insert(
			"evil_fact".into(),
			ent("evil_fact", vec![1.0, 0.0], 0, true),
		);
		g.kerns.insert("remote-evilnet-k1".into(), phantom);

		let ids: Vec<String> = seed_important(&g, &cfg(), &[1.0, 0.0], None)
			.into_iter()
			.map(|h| h.entity_id)
			.collect();

		assert!(
			ids.contains(&"local_fact".to_string()),
			"a LOCAL Fact still bypasses the access gate: {ids:?}"
		);
		assert!(
			!ids.contains(&"evil_fact".to_string()),
			"a REMOTE Fact is gated on access it never earned locally: {ids:?}"
		);
	}

	#[test]
	fn seed_important_applies_cosine_and_access_gates() {
		let g = graph_with(vec![
			ent("hot", vec![1.0, 0.0], 10, false),
			ent("cold", vec![1.0, 0.0], 0, false),
			ent("fact", vec![1.0, 0.0], 0, true),
			ent("off", vec![0.0, 1.0], 10, false),
		]);
		let hits = seed_important(&g, &cfg(), &[1.0, 0.0], None);
		let ids: std::collections::HashSet<&str> = hits.iter().map(|h| h.entity_id.as_str()).collect();
		assert!(ids.contains("hot"), "accessed + aligned is important");
		assert!(
			ids.contains("fact"),
			"a Fact is important regardless of access count"
		);
		assert!(!ids.contains("cold"), "low-access non-fact is dominated");
		assert!(
			!ids.contains("off"),
			"below the cosine threshold is excluded"
		);
	}

	#[test]
	fn seed_important_respects_an_active_filter_at_source() {
		let g = graph_with(vec![
			ent("the_fact", vec![1.0, 0.0], 0, true),
			ent("the_claim", vec![1.0, 0.0], 10, false),
		]);
		let both = seed_important(&g, &cfg(), &[1.0, 0.0], None);
		assert_eq!(both.len(), 2, "no filter keeps both important entities");

		let opts = QueryOptions {
			kind: Some(EntityKind::Fact),
			..Default::default()
		};
		let facts_only = seed_important(&g, &cfg(), &[1.0, 0.0], Some(&opts));
		let ids: Vec<&str> = facts_only.iter().map(|h| h.entity_id.as_str()).collect();
		assert_eq!(
			ids,
			vec!["the_fact"],
			"kind=Fact filter drops the Claim at the source"
		);
	}

	#[test]
	fn active_kind_filter_seeds_matches_post_filtering_would_miss() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		for i in 0..30 {
			let e = ent(&format!("claim{i}"), vec![1.0, 0.0], 0, false);
			k.entities.insert(e.id.clone(), e);
		}
		for i in 0..3 {
			let e = ent(&format!("fact{i}"), vec![0.9, 0.1], 0, true);
			k.entities.insert(e.id.clone(), e);
		}
		g.kerns.insert("kx".into(), k);
		g.rebuild_index();

		let cfg = RetrievalConfig {
			important_min_cosine: 1.5,
			seed_k: 5,
			..Default::default()
		};
		let q = [1.0, 0.0];

		let run = |opts: Option<&QueryOptions>| {
			let important = seed_important(&g, &cfg, &q, opts);
			seed_with_important(&g, &cfg, &q, 5, Mode::Content, opts, &important)
		};

		let unfiltered = run(None);
		assert!(
			unfiltered.iter().all(|h| h.entity_id.starts_with("claim")),
			"unfiltered dense seed is all Claims: {:?}",
			unfiltered.iter().map(|h| &h.entity_id).collect::<Vec<_>>()
		);

		let opts = QueryOptions {
			kind: Some(EntityKind::Fact),
			..Default::default()
		};
		let filtered = run(Some(&opts));
		assert!(
			!filtered.is_empty() && filtered.iter().all(|h| h.entity_id.starts_with("fact")),
			"filtered seed returns only matching Facts: {:?}",
			filtered.iter().map(|h| &h.entity_id).collect::<Vec<_>>()
		);
	}

	#[test]
	fn unfiltered_seed_is_unchanged_when_opts_is_inactive() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		for i in 0..6 {
			let e = ent(&format!("e{i}"), vec![1.0, i as f32 * 0.01], 0, false);
			k.entities.insert(e.id.clone(), e);
		}
		g.kerns.insert("kx".into(), k);
		g.rebuild_index();
		let cfg = RetrievalConfig {
			important_min_cosine: 1.5,
			seed_k: 4,
			..Default::default()
		};
		let q = [1.0, 0.0];

		let run = |opts: Option<&QueryOptions>| {
			let important = seed_important(&g, &cfg, &q, opts);
			seed_with_important(&g, &cfg, &q, 4, Mode::Content, opts, &important)
		};
		let inactive = QueryOptions::default();
		let none = run(None);
		let empty = run(Some(&inactive));
		let ids = |v: &[EntityHit]| v.iter().map(|h| h.entity_id.clone()).collect::<Vec<_>>();
		assert_eq!(
			ids(&none),
			ids(&empty),
			"inactive filter == unfiltered path"
		);
	}

	#[test]
	fn seed_important_is_deterministic_at_scale() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		for i in 0..3000 {
			let a = (i as f32 * 0.001).sin();
			let b = (i as f32 * 0.001).cos();
			let e = ent(&format!("e{i}"), vec![a, b], (i % 7) as u64, i % 5 == 0);
			k.entities.insert(e.id.clone(), e);
		}
		g.kerns.insert("kx".into(), k);
		let cfg = RetrievalConfig {
			important_min_cosine: 0.1,
			important_access_threshold: 3,
			..Default::default()
		};
		let q = [0.6f32, 0.8];
		let a = seed_important(&g, &cfg, &q, None);
		let b = seed_important(&g, &cfg, &q, None);
		assert!(
			a.len() > 50,
			"the scan surfaces a non-trivial set: {}",
			a.len()
		);
		assert_eq!(a.len(), b.len(), "same count across calls");
		for (x, y) in a.iter().zip(b.iter()) {
			assert_eq!(x.entity_id, y.entity_id, "same id order across calls");
			assert_eq!(x.score, y.score, "same score across calls");
		}
	}

	#[test]
	fn merge_seeds_pools_by_entity_and_sorts_descending() {
		let a = vec![EntityHit {
			entity_id: "x".into(),
			score: 0.6,
		}];
		let b = vec![
			EntityHit {
				entity_id: "x".into(),
				score: 0.8,
			},
			EntityHit {
				entity_id: "y".into(),
				score: 0.3,
			},
		];
		let out = merge_seeds(a, b);
		assert_eq!(out.len(), 2, "duplicate id x collapses to a single hit");
		assert_eq!(
			out[0].entity_id, "x",
			"the higher-scoring entity sorts first"
		);
		assert!(out[0].score >= out[1].score, "descending by score");
	}

	#[test]
	fn parallel_importance_scan_equals_the_sequential_scan_it_replaces() {
		let q = vec![0.4f32, -0.2, 0.7, 0.1, -0.5, 0.3, 0.0, 0.6];
		let fact_only = QueryOptions {
			kind: Some(EntityKind::Fact),
			..Default::default()
		};
		let mut compared = 0usize;
		for seed in 0u64..6 {
			let g = generated_graph(seed, 2400);
			for (min_cos, threshold) in [(-1.0, 0), (-0.2, 3), (0.25, 4), (0.8, 6)] {
				let cfg = RetrievalConfig {
					important_min_cosine: min_cos,
					important_access_threshold: threshold,
					..Default::default()
				};
				for opts in [None, Some(&fact_only)] {
					let got = seed_important(&g, &cfg, &q, opts);
					let want = reference_important(&g, &cfg, &q, opts);
					assert_eq!(
						got.len(),
						want.len(),
						"seed {seed} min_cos {min_cos} threshold {threshold} filtered {}: selection size differs",
						opts.is_some()
					);
					for (a, b) in got.iter().zip(want.iter()) {
						assert_eq!(a.entity_id, b.0, "same id in the same rank position");
						// Bit-exact: a reordered sum would be a different scan, not a faster one.
						assert_eq!(a.score.to_bits(), b.1.to_bits(), "same score for {}", b.0);
					}
					compared += got.len();
				}
			}
		}
		assert!(
			compared > 5000,
			"the property compared a non-trivial selection: {compared}"
		);
	}

	// The generated graph above stays under PARALLEL_SCAN_MIN_ENTITIES, so it proves
	// the gate and the serial branch. One kern over the threshold is the only way to
	// reach the in-kern split at all — without this, both branches could disagree
	// forever and every test here would still pass.
	#[test]
	fn the_in_kern_parallel_split_selects_exactly_what_the_serial_walk_does() {
		let n = PARALLEL_SCAN_MIN_ENTITIES + 1_000;
		let mut s = 0x9E3779B97F4A7C15u64;
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		for i in 0..n {
			let vector: Vec<f32> = (0..8)
				.map(|_| (xorshift(&mut s) % 2000) as f32 / 1000.0 - 1.0)
				.collect();
			let e = ent(
				&format!("e{i:06}"),
				vector,
				xorshift(&mut s) % 8,
				i % 4 == 0,
			);
			k.entities.insert(e.id.clone(), e);
		}
		g.kerns.insert("kx".into(), k);
		assert!(
			g.all()
				.iter()
				.any(|k| k.entities.len() >= PARALLEL_SCAN_MIN_ENTITIES),
			"the corpus must cross the split threshold or this test exercises the serial branch"
		);

		let q = vec![0.4f32, -0.2, 0.7, 0.1, -0.5, 0.3, 0.0, 0.6];
		for (min_cos, threshold) in [(-0.2, 3), (0.5, 4)] {
			let cfg = RetrievalConfig {
				important_min_cosine: min_cos,
				important_access_threshold: threshold,
				..Default::default()
			};
			let got = seed_important(&g, &cfg, &q, None);
			let want = reference_important(&g, &cfg, &q, None);
			assert_eq!(
				got.len(),
				want.len(),
				"min_cos {min_cos}: selection size differs"
			);
			for (a, b) in got.iter().zip(want.iter()) {
				assert_eq!(a.entity_id, b.0, "same id in the same rank position");
				assert_eq!(a.score.to_bits(), b.1.to_bits(), "same score for {}", b.0);
			}
			assert!(!got.is_empty(), "min_cos {min_cos} selected something");
		}
	}

	#[test]
	fn an_eligibility_change_is_reflected_with_no_epoch_bump() {
		// The access stamp on delivery (`score::commit_access_ids`) mutates
		// access_count WITHOUT bumping mutation_epoch, by design. Any memo of the
		// importance gate keyed on that epoch is therefore stale forever for the one
		// mutation that creates importance in the first place. This pins the scan
		// against that.
		let cfg = cfg();
		let q = [1.0f32, 0.0];
		let ids = |g: &GraphGnn| -> Vec<String> {
			seed_important(g, &cfg, &q, None)
				.into_iter()
				.map(|h| h.entity_id)
				.collect()
		};
		let mut g = graph_with(vec![
			ent("climber", vec![1.0, 0.0], 2, false),
			ent("demoted", vec![1.0, 0.0], 0, true),
		]);
		let epoch = g.mutation_epoch();
		assert_eq!(
			ids(&g),
			vec!["demoted".to_string()],
			"the Fact starts important, the under-threshold Claim does not"
		);

		g.kerns
			.get_mut("kx")
			.unwrap()
			.entities
			.get_mut("climber")
			.unwrap()
			.access_count
			.increment("t", 3);
		assert_eq!(
			g.mutation_epoch(),
			epoch,
			"an access stamp does not bump the epoch — the premise of this test"
		);
		assert!(
			ids(&g).contains(&"climber".to_string()),
			"crossing the access threshold makes an entity important on the very next retrieve"
		);

		g.kerns
			.get_mut("kx")
			.unwrap()
			.entities
			.get_mut("demoted")
			.unwrap()
			.kind = EntityKind::Claim;
		assert_eq!(g.mutation_epoch(), epoch, "still no epoch bump");
		assert!(
			!ids(&g).contains(&"demoted".to_string()),
			"a Fact demoted to an unaccessed Claim stops being important immediately"
		);
	}

	// ponytail: item-25 guard — pins the four non-access mutation sites item 25
	// names (merge_remote_entity, reembed values_mut, gossip phantom-kern insert,
	// do_cluster move_entity) as epoch-silent. A future chokepoint fix that
	// bump_mutation_epoch()s at one of them fails loudly here instead of shipping
	// a half-bumped eligible-set index. Companion to
	// an_eligibility_change_is_reflected_with_no_epoch_bump (the access site).
	#[test]
	fn non_access_mutations_leave_mutation_epoch_unchanged() {
		use crate::merge::merge_remote_entity;
		use crate::reason::move_entity;

		// (1) merge_remote_entity — insert a remote Fact into a phantom kern.
		{
			let mut g = graph_with(vec![ent("local", vec![1.0, 0.0], 0, true)]);
			g.kerns
				.insert("remote-net-k1".into(), Kern::new("remote-net-k1", ""));
			let before = g.mutation_epoch();
			let remote = ent("remote-fact", vec![1.0, 0.0], 0, true);
			assert!(
				merge_remote_entity(&mut g, "remote-net-k1", remote),
				"merge_remote_entity inserted the remote Fact"
			);
			assert_eq!(
				g.mutation_epoch(),
				before,
				"merge_remote_entity does not bump the mutation_epoch"
			);
		}

		// (2) reembed values_mut shape — replace an entity's vector in place,
		// the mutation do_reembed drives through `kerns.values_mut()`.
		{
			let mut g = graph_with(vec![ent("e", vec![1.0, 0.0], 0, true)]);
			let before = g.mutation_epoch();
			let kern = g.kerns.get_mut("kx").unwrap();
			let e = kern.entities.get_mut("e").unwrap();
			e.vector = vec![0.0f32, 1.0].into();
			assert_eq!(
				g.mutation_epoch(),
				before,
				"reembed's values_mut vector replace does not bump the mutation_epoch"
			);
		}

		// (3) inject_remote_scope / new_phantom_kern shape — gossip writes a
		// phantom-kern entity directly via kerns.insert + entities.insert.
		{
			let mut g = graph_with(vec![ent("local", vec![1.0, 0.0], 0, true)]);
			let before = g.mutation_epoch();
			let mut phantom = Kern::new("remote-net-k2", "");
			phantom.entities.insert(
				"gossip-fact".into(),
				ent("gossip-fact", vec![1.0, 0.0], 0, true),
			);
			g.kerns.insert("remote-net-k2".into(), phantom);
			assert_eq!(
				g.mutation_epoch(),
				before,
				"gossip phantom-kern insert does not bump the mutation_epoch"
			);
		}

		// (4) do_cluster move shape — clustering moves entities between kerns via
		// reason::move_entity.
		{
			let mut g = graph_with(vec![ent("mover", vec![1.0, 0.0], 0, true)]);
			g.kerns.insert("dest".into(), Kern::new("dest", ""));
			let before = g.mutation_epoch();
			move_entity(&mut g, "kx", "dest", "mover").expect("move_entity relocates");
			assert_eq!(
				g.mutation_epoch(),
				before,
				"do_cluster's move_entity does not bump the mutation_epoch"
			);
		}
	}
}
