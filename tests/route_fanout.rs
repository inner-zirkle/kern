// The instrument behind ROADMAP item 31's surviving bullet: per-parent fan-out
// in routing. `route_to_child_id` (`src/base/accept.rs`) is a linear scan over a
// parent's loaded named children, run once per accept, up to MAX_ACCEPT_DEPTH
// times. This measures ingest throughput against root fan-out width so the
// "cliff" claim is a number and not a shape argument.
//
//   cargo test --release --test route_fanout -- --ignored --nocapture --test-threads=1
//
// `--test-threads=1` is not optional: `root_width_growth` saturates the rayon
// pool and inflates every timing here by 2-3x if the two run together.
//
// Ignored by default: the widest configuration accepts into a 20k-entity graph
// and takes minutes in release, unbounded in debug.
use base::base_constants::{KERN_INNER_RADIUS, KERN_OUTER_RADIUS};
use base::base_types::{ChunkPart, ChunkPartKind, Entity, EntityKind, Kern};
use graph::accept::accept_with_dedup;
use graph::graph::GraphGnn;
use std::time::Instant;

const DIM: usize = 384;

// Never dedup: a deduped accept returns before routing, which is the thing under
// measurement.
const NO_DEDUP: f64 = 1.1;

const WORDS: [&str; 32] = [
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
];

fn hash_tok(tok: &str) -> (usize, bool) {
	let mut h: u64 = 1469598103934665603;
	for b in tok.as_bytes() {
		h ^= *b as u64;
		h = h.wrapping_mul(1099511628211);
	}
	((h % DIM as u64) as usize, h & 0x100 != 0)
}

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

fn entity(i: usize) -> Entity {
	let w = words_for(i);
	Entity {
		id: format!("e{i:08}"),
		vector: sparse_vec(&w).into(),
		kind: EntityKind::Claim,
		chunks: vec![ChunkPart {
			kind: ChunkPartKind::Context,
			text: w.join(" "),
			index: 0,
		}],
		..Default::default()
	}
}

/// Root with `width` named gravitons, each carrying a real `graviton_vec` drawn
/// from the same hashed-word space as the entities, so the scan does the full
/// cosine per child exactly as it would in production.
fn build(width: usize, prepop: usize, named: bool) -> GraphGnn {
	let mut g = GraphGnn::new();
	let root_id = g.root.id.clone();
	let net = g.root.root_id.clone();

	let mut graviton_ids = Vec::with_capacity(width);
	for w in 0..width {
		let id = format!("grav{w:06}");
		let mut k = Kern::new(&id, &root_id);
		k.root_id = net.clone();
		// `named` toggles the only per-child work the scan itself does: an unnamed
		// child is rejected before the cosine, so the delta between the two sweeps
		// IS the cost of the graviton_vec comparison the item names.
		if named {
			k.graviton_text = format!("topic {w}");
			k.graviton_vec = sparse_vec(&words_for(1_000_000 + w));
		}
		k.inner_radius = KERN_INNER_RADIUS;
		k.outer_radius = KERN_OUTER_RADIUS;
		graviton_ids.push(id.clone());
		g.register(k);
		if let Some(r) = g.get_mut(&root_id) {
			r.children.push(id);
		}
	}

	// Population lives under the gravitons so the global entity index — which the
	// dedup gate and the similarity-reason gate both search — is realistically
	// sized and identical across widths.
	for i in 0..prepop {
		let e = entity(2_000_000 + i);
		let host = if graviton_ids.is_empty() {
			root_id.clone()
		} else {
			graviton_ids[i % graviton_ids.len()].clone()
		};
		let id = e.id.clone();
		let vec = e.vector.clone();
		g.entity_idx.insert(id.clone(), vec);
		if let Some(k) = g.get_mut(&host) {
			k.entities.insert(id.clone(), e);
		}
		g.index_entity(&id, &host);
	}
	g
}

fn run(width: usize, prepop: usize, reps: usize, named: bool) -> f64 {
	let mut g = build(width, prepop, named);
	let root_id = g.root.id.clone();

	// Warm: the first accepts pay one-time index growth, not routing.
	for i in 0..50 {
		let r = accept_with_dedup(&mut g, &root_id, entity(3_000_000 + i), "", NO_DEDUP);
		std::hint::black_box(r);
	}

	let t = Instant::now();
	for i in 0..reps {
		let r = accept_with_dedup(&mut g, &root_id, entity(4_000_000 + i), "", NO_DEDUP);
		std::hint::black_box(r);
	}
	let us = t.elapsed().as_secs_f64() * 1e6 / reps as f64;
	let kind = if named { "named" } else { "unnamed" };
	println!("  width={width:<6} prepop={prepop:<7} children={kind:<8} accept={us:9.1}us/entity");
	us
}

#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn fanout() {
	const REPS: usize = 400;
	const WIDTHS: [usize; 5] = [1, 8, 64, 512, 4096];

	println!("empty graph — width cost with nothing else to pay for:");
	let bare: Vec<(usize, f64)> = WIDTHS
		.iter()
		.map(|w| (*w, run(*w, 0, REPS, true)))
		.collect();

	println!("empty graph, children unnamed — same walk, cosine skipped:");
	let bare_unnamed: Vec<(usize, f64)> = WIDTHS
		.iter()
		.map(|w| (*w, run(*w, 0, REPS, false)))
		.collect();

	println!("20k-entity graph — production shape:");
	let full: Vec<(usize, f64)> = WIDTHS
		.iter()
		.map(|w| (*w, run(*w, 20_000, REPS, true)))
		.collect();

	let span = (WIDTHS[4] - WIDTHS[0]) as f64;
	let slope = (bare[4].1 - bare[0].1) / span;
	let slope_unnamed = (bare_unnamed[4].1 - bare_unnamed[0].1) / span;
	println!("\nper-child cost of fan-out:  {slope:.4}us (named, cosine runs)");
	println!("per-child cost of fan-out:  {slope_unnamed:.4}us (unnamed, cosine skipped)");
	println!(
		"attributable to the graviton_vec comparison: {:.4}us/child\n",
		slope - slope_unnamed
	);

	for (w, us) in &full {
		let routing = slope * *w as f64;
		println!(
			"  width={w:<6} fan-out~{routing:8.1}us of {us:9.1}us total = {:5.1}% of ingest",
			100.0 * routing / us
		);
	}

	// Guards the instrument, not a behaviour: a flat sweep means the widths never
	// reached the routing walk and every percentage above it is noise.
	assert!(
		slope > 0.02,
		"fan-out width has no measurable cost ({slope:.4}us/child) — the sweep is not reaching route_entity"
	);
}

// The other half of the question: a width nothing reaches is not a hot loop.
// Root fan-out grows by exactly one path — a cohesive cluster inside `generic`
// spawns an unnamed child, tick naming gives it a graviton, and
// `promote_to_root_if_generic` lifts it to root unless an existing root graviton
// is within GRAVITON_DEDUP_THRESHOLD. This drives that whole loop with a fake
// LLM and embedder and reports how many distinct topics survive to root.
//
// Topic tokens are unique per topic rather than drawn from WORDS: a shared
// vocabulary makes distinct topics produce the SAME three-word graviton name,
// which the 0.85 promotion gate then collapses — measuring the generator, not
// the routing structure.
fn topic_words(t: usize) -> Vec<String> {
	(0..4).map(|k| format!("tpc{t}x{k}")).collect()
}

fn topic_entity(t: usize, i: usize) -> Entity {
	let mut w = topic_words(t);
	w.push(WORDS[(i * 7 + t) % WORDS.len()].to_string());
	let refs: Vec<&str> = w.iter().map(String::as_str).collect();
	Entity {
		id: format!("t{t:05}_{i:05}"),
		vector: sparse_vec(&refs).into(),
		kind: EntityKind::Claim,
		chunks: vec![ChunkPart {
			kind: ChunkPartKind::Context,
			text: w.join(" "),
			index: 0,
		}],
		..Default::default()
	}
}

#[test]
#[ignore = "drives the real cluster+name+promote loop; minutes in release"]
fn root_width_growth() {
	use kern::llm::{EmbedFunc, LlmFunc};
	use parking_lot::RwLock;
	use std::sync::Arc;

	// Stands in for the naming LLM: the three most frequent words across the
	// cluster's sampled member texts, which is what a real summariser converges on
	// for these synthetic topics.
	let llm: LlmFunc = Arc::new(|prompt: &str| {
		let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
		for line in prompt.lines().filter_map(|l| l.strip_prefix("- ")) {
			for tok in line.split_whitespace() {
				*counts.entry(tok).or_default() += 1;
			}
		}
		let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
		ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
		ranked
			.iter()
			.take(3)
			.map(|(w, _)| *w)
			.collect::<Vec<_>>()
			.join(" ")
	});
	let embed: EmbedFunc = Arc::new(|text: &str| {
		let w: Vec<&str> = text.split_whitespace().collect();
		Ok(sparse_vec(&w))
	});

	for topics in [8usize, 64, 256] {
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let root_id = g.read().root.id.clone();

		// Topics arrive one at a time with a tick between them — the shape that
		// maximises spawning.
		for t in 0..topics {
			for i in 0..16 {
				let mut w = g.write();
				let r = accept_with_dedup(&mut w, &root_id, topic_entity(t, i), "", NO_DEDUP);
				std::hint::black_box(r);
			}
			// do_cluster only cascades into children it just spawned, so every
			// resident kern is ticked explicitly — the daemon reaches them via the pulse.
			for _ in 0..2 {
				let ids: Vec<String> = g.read().kerns.keys().cloned().collect();
				for id in ids {
					kern::tick::tick_sync(&g, &id, Some(&llm), Some(&embed), None);
				}
			}
		}

		let gr = g.read();
		let kids = gr
			.loaded(&root_id)
			.map(|k| k.children.clone())
			.unwrap_or_default();
		let routable = kids
			.iter()
			.filter(|c| {
				gr.loaded(c)
					.map(|k| !k.graviton_text.is_empty() && !k.graviton_vec.is_empty())
					.unwrap_or(false)
			})
			.count();
		let widest = gr
			.kerns
			.values()
			.map(|k| k.children.len())
			.max()
			.unwrap_or(0);
		println!(
			"topics={topics:<5} root children={:<5} routable named children={routable:<5} widest parent={widest:<5} kerns={}",
			kids.len(),
			gr.count()
		);

		// The claim under test: nothing suppresses root fan-out. It tracks the
		// count of distinct cohesive topics, so the width the sweep above prices
		// is a width the graph really reaches. `GRAVITON_DEDUP_THRESHOLD` collapses
		// only topics whose graviton names embed within 0.85 of each other, which
		// is a statement about the corpus, not a bound on the structure.
		if topics >= 128 {
			assert!(
				routable * 2 >= topics,
				"root fan-out {routable} did not track {topics} distinct topics — the promote path stopped feeding root and the width sweep prices a shape nothing reaches"
			);
		}
	}
}
