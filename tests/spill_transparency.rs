// Does a spill change what a query finds? The safety claim behind ROADMAP item
// 29 is that spilling is a residency decision, not a retrieval decision. This
// runs the SAME queries against a graph that spilled and one that never did and
// compares the ranked ids.
//
//   cargo test --release --test spill_transparency -- --nocapture
use kern::base_constants::KERN_CAP_DISABLED;
use kern::base_types::{ChunkPart, ChunkPartKind, Entity, EntityKind, Kern};
use kern::graph::GraphGnn;
use kern::search::search_all_unlocked;
use kern::vector_backend::VectorBackend;

const DIM: usize = 64;
const N: usize = 1_000;

fn hash_tok(tok: &str) -> (usize, bool) {
	let mut h: u64 = 1469598103934665603;
	for b in tok.as_bytes() {
		h ^= *b as u64;
		h = h.wrapping_mul(1099511628211);
	}
	((h % DIM as u64) as usize, h & 0x100 != 0)
}

fn sparse_vec(seed: usize) -> Vec<f32> {
	let mut v = vec![0.0f32; DIM];
	for j in 0..7 {
		let (i, pos) = hash_tok(&format!(
			"w{}",
			seed.wrapping_mul(2654435761).wrapping_add(j)
		));
		v[i] += if pos { 1.0 } else { -1.0 };
	}
	let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
	for x in &mut v {
		*x /= n;
	}
	v
}

fn build(dir: &std::path::Path, threshold: usize) -> GraphGnn {
	let mut g = GraphGnn::new();
	g.data_dir = dir.to_string_lossy().into_owned();
	let mut k = Kern::new("kx", "");
	for i in 0..N {
		let v = sparse_vec(i);
		let e = Entity {
			id: format!("e{i:07}"),
			gnn_vector: v.clone().into(),
			vector: v.into(),
			kind: EntityKind::Claim,
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::Context,
				text: format!("entity {i}"),
				index: 0,
			}],
			..Default::default()
		};
		k.entities.insert(e.id.clone(), e);
	}
	g.kerns.insert("kx".into(), k);
	g.set_disk_threshold(threshold);
	g.rebuild_index();
	g
}

fn top_ids(g: &GraphGnn, seed: usize, k: usize) -> Vec<String> {
	search_all_unlocked(g, &sparse_vec(seed), k)
		.into_iter()
		.map(|h| h.entity_id)
		.collect()
}

// The reference both backends are approximations of: exact cosine over every
// indexed vector, ranked by the same `cmp_rank` the backends use, so a tie
// breaks identically and a disagreement is a real ranking difference.
fn brute_ids(g: &GraphGnn, seed: usize, k: usize) -> Vec<String> {
	let q = sparse_vec(seed);
	let mut scored: Vec<(String, f64)> = g
		.kerns
		.values()
		.flat_map(|kern| kern.entities.values())
		.map(|e| {
			let dot: f32 = e.vector.iter().zip(q.iter()).map(|(a, b)| a * b).sum();
			(e.id.clone(), dot as f64)
		})
		.collect();
	scored.sort_by(|a, b| kern::util::cmp_rank(a.1, &a.0, b.1, &b.0));
	scored.truncate(k);
	scored.into_iter().map(|(id, _)| id).collect()
}

fn overlap(a: &[String], b: &[String]) -> usize {
	a.iter().filter(|id| b.contains(id)).count()
}

#[test]
fn a_spilled_graph_answers_the_same_queries_as_one_that_never_spilled() {
	let hot_dir = tempfile::tempdir().unwrap();
	let cold_dir = tempfile::tempdir().unwrap();
	let hot = build(hot_dir.path(), KERN_CAP_DISABLED);
	let cold = build(cold_dir.path(), 10);

	assert!(
		matches!(hot.entity_idx, VectorBackend::Resident(_)),
		"precondition: the control never spilled"
	);
	assert!(
		matches!(cold.entity_idx, VectorBackend::Disk { .. }),
		"precondition: the subject did spill"
	);
	// The inline `the_same_corpus_builds_a_byte_identical_index` runs a 600x64
	// corpus so the debug suite stays fast; this is the same claim at the size and
	// shape a spill actually happens on, through `rebuild_index` rather than
	// through `build_and_save` directly.
	let repeat_dir = tempfile::tempdir().unwrap();
	let repeat = build(repeat_dir.path(), 10);
	let graph_of = |d: &std::path::Path| {
		std::fs::read(d.join("diskann").join("entity").join("graph.bin")).unwrap()
	};
	let (a, b) = (graph_of(cold_dir.path()), graph_of(repeat_dir.path()));
	let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
	assert_eq!(
		differing,
		0,
		"two spills of one corpus produced different adjacency ({differing} of {} bytes)",
		a.len()
	);
	drop(repeat);

	const K: usize = 10;
	const QUERIES: usize = 200;
	let mut agree = 0usize;
	let mut hot_vs_brute = 0usize;
	let mut cold_vs_brute = 0usize;
	let mut top1 = 0usize;
	for q in 0..QUERIES {
		let seed = q * 977 + 5;
		let a = top_ids(&hot, seed, K);
		let b = top_ids(&cold, seed, K);
		let x = brute_ids(&hot, seed, K);
		agree += overlap(&a, &b);
		hot_vs_brute += overlap(&a, &x);
		cold_vs_brute += overlap(&b, &x);
		if a.first() == b.first() {
			top1 += 1;
		}
	}
	let denom = (QUERIES * K) as f64;
	println!(
		"recall@{K} vs brute force: resident={:.4} spilled={:.4}; spilled-vs-resident overlap={:.4}, top1 agreement={:.4}",
		hot_vs_brute as f64 / denom,
		cold_vs_brute as f64 / denom,
		agree as f64 / denom,
		top1 as f64 / QUERIES as f64,
	);

	let hot_recall = hot_vs_brute as f64 / denom;
	let cold_recall = cold_vs_brute as f64 / denom;

	// NOT equality, and the reason IS the finding: spilling swaps one approximate
	// index for another, so identical answers were never on offer. RECORDED
	// BASELINE, NOT A TARGET, in the shape `tests/e2e/test_recall.py` uses — measured
	// 2026-07-21 at resident 1.0000, spilled 0.9940, overlap 0.9940, and rounded
	// down. These are reruns rather than samples: the byte check above is what
	// makes both builds reproducible.
	assert!(
		hot_recall >= 0.99,
		"resident HNSW recall@{K} vs brute force regressed: {hot_recall:.4}"
	);
	assert!(
		cold_recall >= 0.99,
		"spilled DiskANN recall@{K} vs brute force regressed: {cold_recall:.4}"
	);
	assert!(
		hot_recall - cold_recall <= 0.01,
		"spilling cost more than 1pp of recall@{K}: {hot_recall:.4} -> {cold_recall:.4}"
	);
	assert!(
		agree as f64 / denom >= 0.99,
		"spilled and resident answers diverged beyond the measured baseline"
	);
}
