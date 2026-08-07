// The scaling instrument behind ROADMAP items 25 and 26. Ignored by default:
// it builds a 100k-entity graph and runs 20 retrievals per configuration, which
// is ~11 minutes in release and effectively unbounded in debug — running it under
// a plain `cargo test` is what stalled the first attempt at item 25.
//
//   cargo test --release --test seed_scale -- --ignored --nocapture
//
// Kept rather than deleted because both items are perf claims, and a perf claim
// whose instrument was thrown away cannot be rechecked when the numbers move.
use base::base_types::{ChunkPart, ChunkPartKind, Entity, EntityKind, Kern, Reason};
use graph::graph::GraphGnn;
use kern::config::RetrievalConfig;
use kern::retrieval::query::retrieve_profiled;
use kern::retrieval::seed::{seed_important, Mode, Weights};
use std::time::Instant;

const DIM: usize = 384;

const WORDS: [&str; 48] = [
	"ada",
	"bicycle",
	"garden",
	"shed",
	"marrow",
	"cat",
	"salmon",
	"quill",
	"parrot",
	"doorbell",
	"dog",
	"walker",
	"eleven",
	"weekday",
	"bee",
	"lavender",
	"rose",
	"courtyard",
	"tomato",
	"sunlight",
	"whale",
	"cape",
	"november",
	"sourdough",
	"starter",
	"kettle",
	"attic",
	"ledger",
	"harbor",
	"violin",
	"compost",
	"lantern",
	"meridian",
	"quarry",
	"saffron",
	"trellis",
	"umber",
	"vellum",
	"wharf",
	"xenon",
	"yarrow",
	"zephyr",
	"anvil",
	"basalt",
	"cinder",
	"dovetail",
	"ember",
	"fathom",
];

fn hash_tok(tok: &str) -> (usize, bool) {
	let mut h: u64 = 1469598103934665603;
	for b in tok.as_bytes() {
		h ^= *b as u64;
		h = h.wrapping_mul(1099511628211);
	}
	((h % DIM as u64) as usize, h & 0x100 != 0)
}

// Same shape as tests/e2e/fake_llm.py: feature-hashed bag of words, L2-normalised.
fn sparse_vec(words: &[&str]) -> Vec<f32> {
	let mut v = vec![0.0f32; DIM];
	for w in words {
		let (i, pos) = hash_tok(w);
		v[i] += if pos { 1.0 } else { -1.0 };
	}
	let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
	for x in &mut v {
		*x /= n;
	}
	v
}

fn words_for(i: usize) -> Vec<&'static str> {
	let mut out = Vec::new();
	let mut s = i as u64 * 2654435761;
	for _ in 0..7 {
		s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
		out.push(WORDS[(s >> 40) as usize % WORDS.len()]);
	}
	out
}

// `eligible_pct` = share of entities that clear the query-INDEPENDENT importance
// gates (a local Fact, or a Claim with access_count >= threshold). kern's own
// ingest path only ever writes Claims (`src/ingest/place.rs`), and a Claim earns
// access only by being delivered, so on a real corpus this share is small.
fn build(n: usize, eligible_pct: usize) -> GraphGnn {
	let mut g = GraphGnn::new();
	let mut k = Kern::new("kx", "");
	for i in 0..n {
		let w = words_for(i);
		let eligible = i * 100 / n.max(1) % 100 < eligible_pct;
		let fact = eligible && i % 5 == 0;
		let mut e = Entity {
			id: format!("e{i:07}"),
			vector: sparse_vec(&w).into(),
			kind: if fact {
				EntityKind::Fact
			} else {
				EntityKind::Claim
			},
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::Context,
				text: w.join(" "),
				index: 0,
			}],
			..Default::default()
		};
		if eligible && !fact {
			e.access_count.increment("t", 3 + (i % 5) as u64);
		}
		k.entities.insert(e.id.clone(), e);
	}
	for i in 0..n / 2 {
		let r = Reason {
			id: format!("r{i}"),
			from: format!("e{i:07}"),
			to: format!("e{:07}", (i * 7 + 3) % n),
			..Default::default()
		};
		k.by_from
			.entry(r.from.clone())
			.or_default()
			.push(r.id.clone());
		k.by_to.entry(r.to.clone()).or_default().push(r.id.clone());
		k.reasons.insert(r.id.clone(), r);
	}
	g.kerns.insert("kx".into(), k);
	g.rebuild_index();
	if let Some(lex) = g.lexical() {
		lex.rebuild_from_graph(&g);
	}
	g
}

fn run_cfg(g: &GraphGnn, n: usize, pct: usize, tag: &str, cfg: &RetrievalConfig) {
	let w = Weights::for_mode(cfg, Mode::Hybrid);
	let qwords = words_for(12345);
	let qvec = sparse_vec(&qwords);
	let qtext = qwords.join(" ");

	for _ in 0..2 {
		let _ = retrieve_profiled(g, cfg, &qvec, &qtext, Mode::Hybrid, None, w);
	}

	const REPS: usize = 20;
	let t = Instant::now();
	let mut imp_len = 0;
	for _ in 0..REPS {
		let h = seed_important(g, cfg, &qvec, None);
		imp_len = h.len();
		std::hint::black_box(h);
	}
	let imp_ms = t.elapsed().as_secs_f64() * 1000.0 / REPS as f64;

	let t = Instant::now();
	let mut prof = None;
	for _ in 0..REPS {
		let (r, p) = retrieve_profiled(g, cfg, &qvec, &qtext, Mode::Hybrid, None, w);
		prof = Some(p);
		std::hint::black_box(r);
	}
	let total_ms = t.elapsed().as_secs_f64() * 1000.0 / REPS as f64;
	let p = prof.unwrap();
	let stages: Vec<String> = p
		.checkpoints
		.iter()
		.map(|c| format!("{}={:.2}", c.label, c.elapsed_ms))
		.collect();

	println!(
		"N={n:<7} eligible={pct:>3}% {tag:<12} seed_important={imp_ms:8.3}ms ({:5.1}% of retrieve) retrieve={total_ms:8.3}ms hits={imp_len:<7} [{}]",
		100.0 * imp_ms / total_ms,
		stages.join(" ")
	);
}

#[test]
#[ignore = "11 minutes in release; run explicitly with --ignored"]
fn scale() {
	for n in [10_000usize, 100_000] {
		for pct in [1usize, 10, 50, 100] {
			let g = build(n, pct);
			run_cfg(&g, n, pct, "default", &RetrievalConfig::default());
			run_cfg(
				&g,
				n,
				pct,
				"no-pagerank",
				&RetrievalConfig {
					pagerank_enabled: false,
					..Default::default()
				},
			);
		}
	}
}
