//! m2_fold_put — kern's graph + store + dedup leg, timed with the embed leg cut out.
//!
//! `docs/specs/25-kern-fold.md` §5 (in the model repo) closed the fold question
//! only halfway: the embed leg is HTTP by architecture, so folding cannot remove
//! that round-trip — but the *remaining* leg was left as an assertion, verbatim
//! "that leg's cost is still not measured in isolation". This binary is that
//! measurement.
//!
//! What is on the timed path, and nothing else:
//!   - dedup scan  — `find_duplicate`, the HNSW search over `entity_idx`
//!   - graph insert — `accept_with_dedup` (route + commit) plus the lexical index
//!
//! What is deliberately off it:
//!   - embedding. Vectors are precomputed, exactly as the ticket requires; an
//!     HTTP call on the timed path would measure the network, which §5 already
//!     settled.
//!   - chunk splitting and job construction, which are per-document, not per-put.
//!
//! Usage: `m2_fold_put [n_puts] [n_prepop] [dim]`
//! Defaults: 2000 puts into a graph pre-populated with 20_000 entities at 1024-d
//! (nomic/qwen3's width — ANN cost scales with dimension, so measuring at 3-d
//! like the unit tests would understate it by orders of magnitude).

use base::base_types::*;
use graph::accept;
use graph::graph::GraphGnn;
use ingest::ingest_dedup::find_duplicate;
use ingest::ingest_place::build_chunk_entity;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Instant;

/// Deterministic unit vectors, spread far enough apart that the dedup scan
/// finds no match. A corpus of near-duplicates would exercise the merge path
/// instead of the insert path, and the insert path is what a "put" costs.
fn vector(seed: u64, dim: usize) -> Vec<f32> {
	let mut s = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
	let mut v = Vec::with_capacity(dim);
	for _ in 0..dim {
		s ^= s << 13;
		s ^= s >> 7;
		s ^= s << 17;
		v.push(((s >> 40) as f32 / (1u64 << 23) as f32) - 1.0);
	}
	let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
	for x in v.iter_mut() {
		*x /= norm;
	}
	v
}

fn entity(i: u64, dim: usize) -> Entity {
	build_chunk_entity(
		&format!("bench statement number {i}"),
		&vector(i, dim),
		EntityKind::Claim,
		&Source::Session {
			session_id: "kf3".into(),
			section: "bench".into(),
			title: String::new(),
		},
		&format!("kf3:{i}"),
		0.9,
		None,
		&Scoping::default(),
	)
}

fn main() {
	let arg = |i: usize, d: usize| -> usize {
		std::env::args()
			.nth(i)
			.and_then(|s| s.parse().ok())
			.unwrap_or(d)
	};
	let n_puts = arg(1, 2_000);
	let n_prepop = arg(2, 20_000);
	let dim = arg(3, 1024);

	let graph = Arc::new(RwLock::new(GraphGnn::new()));
	let root_id = graph.read().root.id.clone();

	// Pre-populate. An ANN index over an empty graph answers in constant time;
	// the number that matters is the cost at a realistic resident size.
	for i in 0..n_prepop as u64 {
		let mut g = graph.write();
		accept::accept_with_dedup(&mut g, &root_id, entity(i, dim), "prepop", 0.95);
	}
	let resident: usize = graph.read().all().iter().map(|k| k.entities.len()).sum();
	eprintln!("prepopulated {resident} entities at {dim}-d");

	// Build the incoming entities up front: allocation is not the leg.
	let base = n_prepop as u64 + 1_000_000;
	let incoming: Vec<Entity> = (0..n_puts as u64).map(|i| entity(base + i, dim)).collect();
	let vecs: Vec<Vec<f32>> = (0..n_puts as u64).map(|i| vector(base + i, dim)).collect();

	// --- leg 1: the dedup scan alone -------------------------------------
	let t0 = Instant::now();
	let mut hits = 0usize;
	for v in vecs.iter() {
		if find_duplicate(&graph, v, 0.95).is_some() {
			hits += 1;
		}
	}
	let dedup_us = t0.elapsed().as_secs_f64() * 1e6 / n_puts as f64;
	assert_eq!(
		hits, 0,
		"bench vectors must not collide, or this times the merge path"
	);

	// --- leg 2: the graph insert alone -----------------------------------
	let t1 = Instant::now();
	for e in incoming {
		let tid = e.id.clone();
		let joined = e.statements.join(" ");
		let (r, lex) = {
			let mut g = graph.write();
			let r = accept::accept_with_dedup(&mut g, &root_id, e, "bench", 0.95);
			let l = g.lexical();
			(r, l)
		};
		if !r.deduped {
			if let Some(lex) = lex {
				lex.insert(&tid, &joined);
			}
		}
	}
	let insert_us = t1.elapsed().as_secs_f64() * 1e6 / n_puts as f64;

	println!("kern_dedup_us {dedup_us:.3}");
	println!("kern_insert_us {insert_us:.3}");
	println!("kern_leg_us {:.3}", dedup_us + insert_us);
	println!("kern_n {n_puts}");
	println!("kern_resident {resident}");
	println!("kern_dim {dim}");
}
