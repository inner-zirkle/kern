// The memory instrument behind ROADMAP item 29, in the shape of
// `tests/seed_scale.rs`. Ignored by default: it builds a 50k-entity graph three
// times over and runs a DiskANN Vamana build, which is minutes in release and
// effectively unbounded in debug.
//
//   cargo test --release --test spill_memory -- --ignored --nocapture
//
// METHOD. Resident size is measured in the GROWTH direction only, and each
// configuration gets its OWN PROCESS. Both are load-bearing: glibc does not
// return the many ~1.5KB vector allocations an HNSW node holds, so measuring a
// spill by dropping a resident index inside one process reads as "no change"
// even when the structure is genuinely gone. The parent re-execs this same test
// binary once per configuration with KERN_SPILL_MODE set and prints the table.
//
// `KERN_SPILL_N` overrides the corpus size.
use kern::base_constants::KERN_CAP_DISABLED;
use kern::base_types::{ChunkPart, ChunkPartKind, Entity, EntityKind, Kern, Reason};
use kern::graph::GraphGnn;
use kern::vector_backend::VectorBackend;

const DIM: usize = 384;
const DEFAULT_N: usize = 50_000;

// Field 2 of /proc/self/statm is resident pages.
fn rss_bytes() -> u64 {
	let s = std::fs::read_to_string("/proc/self/statm").expect("statm");
	let pages: u64 = s.split_whitespace().nth(1).unwrap().parse().unwrap();
	pages * 4096
}

// VmHWM: the high-water mark, which is what a transient build spike costs the
// host even though it is gone by the time the steady state is measured.
fn peak_rss_bytes() -> u64 {
	let s = std::fs::read_to_string("/proc/self/status").expect("status");
	for line in s.lines() {
		if let Some(rest) = line.strip_prefix("VmHWM:") {
			let kb: u64 = rest.split_whitespace().next().unwrap().parse().unwrap();
			return kb * 1024;
		}
	}
	0
}

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

// Mirrors the steady state the ingest path leaves behind: every embedded entity
// carries BOTH `vector` and `gnn_vector` (`src/tick/tasks.rs:541-543` seeds the
// GNN vector from the raw embed), and every enriched reason carries a vector.
fn build_kerns(g: &mut GraphGnn, n: usize) {
	let mut k = Kern::new("kx", "");
	for i in 0..n {
		let v = sparse_vec(i);
		let e = Entity {
			id: format!("e{i:07}"),
			gnn_vector: v.clone().into(),
			vector: v.into(),
			kind: if i.is_multiple_of(5) {
				EntityKind::Fact
			} else {
				EntityKind::Claim
			},
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::Context,
				text: format!("entity {i}"),
				index: 0,
			}],
			..Default::default()
		};
		k.entities.insert(e.id.clone(), e);
	}
	for i in 0..n / 2 {
		let r = Reason {
			id: format!("r{i:07}"),
			from: format!("e{i:07}"),
			to: format!("e{:07}", (i * 7 + 3) % n),
			vector: sparse_vec(n + i).into(),
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
}

#[derive(Clone, Copy)]
enum Index {
	Entity,
	Gnn,
	Reason,
}

// Borrowed so the items are never materialised into a second full copy — that
// copy would land in the number being measured.
fn items(g: &GraphGnn, which: Index) -> Vec<(&str, &[f32])> {
	let mut out = Vec::new();
	for k in g.kerns.values() {
		match which {
			Index::Entity => out.extend(k.entities.values().map(|e| (e.id.as_str(), &e.vector[..]))),
			Index::Gnn => out.extend(
				k.entities
					.values()
					.map(|e| (e.id.as_str(), &e.gnn_vector[..])),
			),
			Index::Reason => out.extend(k.reasons.values().map(|r| (r.id.as_str(), &r.vector[..]))),
		}
	}
	out.sort_by(|a, b| a.0.cmp(b.0));
	out
}

fn fill(g: &GraphGnn, which: Index) -> VectorBackend {
	let mut idx = VectorBackend::resident(16, 200, g.quant_mode);
	for (id, v) in items(g, which) {
		idx.insert(id.to_string(), v.to_vec().into());
	}
	idx
}

fn spill(g: &GraphGnn, which: Index, root: &std::path::Path, name: &str) -> VectorBackend {
	let owned: Vec<(String, Vec<f32>)> = items(g, which)
		.into_iter()
		.map(|(id, v)| (id.to_string(), v.to_vec()))
		.collect();
	let dir = root.join("diskann").join(name);
	kern::diskann::build_and_save(&dir, &owned, kern::diskann::Params::default())
		.expect("snapshot build");
	drop(owned);
	VectorBackend::disk(
		kern::diskann::DiskIndex::open(&dir).expect("snapshot open"),
		g.quant_mode,
	)
}

fn child(mode: &str, n: usize) {
	let dir = tempfile::tempdir().unwrap();
	let mut g = GraphGnn::new();
	g.data_dir = dir.path().to_string_lossy().into_owned();
	build_kerns(&mut g, n);

	match mode {
		"data" => {}
		// One index at a time, so each line of the table is that structure's own
		// resident cost rather than a subtraction between two totals.
		"entity_only" => g.entity_idx = fill(&g, Index::Entity),
		"gnn_only" => g.gnn_entity_idx = fill(&g, Index::Gnn),
		"reason_only" => g.reason_idx = fill(&g, Index::Reason),
		"resident" => {
			g.set_disk_threshold(KERN_CAP_DISABLED);
			g.rebuild_index();
		}
		"spilled" => {
			g.set_disk_threshold(1);
			g.rebuild_index();
		}
		// Prices the fix ROADMAP item 29 asks for WITHOUT shipping it: all three
		// indexes on disk, assembled here from the same public parts
		// `rebuild_index` uses for the entity index alone.
		// `rebuild_index` is deliberately NOT called: building the resident HNSWs
		// first and replacing them leaves glibc holding their arenas, which would
		// be charged to the configuration that never wanted them.
		"spilled_all" => {
			g.entity_idx = spill(&g, Index::Entity, dir.path(), "entity");
			g.gnn_entity_idx = spill(&g, Index::Gnn, dir.path(), "gnn");
			g.reason_idx = spill(&g, Index::Reason, dir.path(), "reason");
		}
		other => panic!("unknown mode {other}"),
	}

	let cold = rss_bytes();
	// mmap-backed pages only count once touched, so a cold reading flatters the
	// disk backend. 200 searches is the honest steady state under query load.
	//
	// The same 200 searches are the latency instrument: sharing the vector
	// between the kern map and the index puts an `Arc` between the distance loop
	// and its floats, and that lands on the retrieval hot path. Queries are
	// pre-embedded so the reading is index walk only. The first 20 are discarded
	// as warm-up (page faults on the first touch of every structure).
	let queries: Vec<Vec<f32>> = (0..200).map(|i| sparse_vec(i * 37)).collect();
	let mut search_ns: u64 = 0;
	for (i, q) in queries.iter().enumerate() {
		let t0 = std::time::Instant::now();
		std::hint::black_box(kern::search::search_all_unlocked(&g, q, 20));
		std::hint::black_box(kern::search::search_reasons_all_unlocked(&g, q, 20));
		if i >= 20 {
			search_ns += t0.elapsed().as_nanos() as u64;
		}
	}
	let search_us = search_ns / 180 / 1000;
	let hot = rss_bytes();
	let peak = peak_rss_bytes();

	println!(
		"RESULT mode={mode} n={n} cold={cold} hot={hot} peak={peak} search_us={search_us} entity_idx={} entity_variant={} gnn_idx={} reason_idx={}",
		g.entity_idx.len(),
		if matches!(g.entity_idx, VectorBackend::Disk { .. }) {
			"disk"
		} else {
			"resident"
		},
		g.gnn_entity_idx.len(),
		g.reason_idx.len(),
	);
	std::hint::black_box(&g);
}

fn run_child(mode: &str, n: usize) -> String {
	let exe = std::env::current_exe().expect("test binary");
	let out = std::process::Command::new(exe)
		.args(["spill_memory_report", "--ignored", "--nocapture", "--exact"])
		.env("KERN_SPILL_MODE", mode)
		.env("KERN_SPILL_N", n.to_string())
		.output()
		.expect("spawn child");
	let text = String::from_utf8_lossy(&out.stdout).into_owned();
	text
		.lines()
		.find(|l| l.starts_with("RESULT "))
		.unwrap_or_else(|| {
			panic!(
				"child mode={mode} produced no RESULT line\nstdout:\n{text}\nstderr:\n{}",
				String::from_utf8_lossy(&out.stderr)
			)
		})
		.to_string()
}

fn field(line: &str, key: &str) -> String {
	line
		.split_whitespace()
		.find_map(|kv| kv.strip_prefix(&format!("{key}=")))
		.unwrap_or_else(|| panic!("missing {key} in {line}"))
		.to_string()
}

fn mb(bytes: u64) -> f64 {
	bytes as f64 / (1024.0 * 1024.0)
}

#[test]
#[ignore = "minutes in release (a Vamana build over the whole corpus); run explicitly with --ignored"]
fn spill_memory_report() {
	let n: usize = std::env::var("KERN_SPILL_N")
		.ok()
		.and_then(|v| v.parse().ok())
		.unwrap_or(DEFAULT_N);

	if let Ok(mode) = std::env::var("KERN_SPILL_MODE") {
		child(&mode, n);
		return;
	}

	let lines: Vec<(String, String)> = [
		"data",
		"entity_only",
		"gnn_only",
		"reason_only",
		"resident",
		"spilled",
		"spilled_all",
	]
	.iter()
	.map(|m| (m.to_string(), run_child(m, n)))
	.collect();

	let base: u64 = field(&lines[0].1, "hot").parse().unwrap();
	println!(
		"\ncorpus: n={n} entities (dim {DIM}, each with vector AND gnn_vector), {} reasons\n",
		n / 2
	);
	println!(
		"{:<19} {:>12} {:>12} {:>12} {:>14} {:>11} {:>10} {:>10} {:>10}",
		"mode", "cold MB", "hot MB", "peak MB", "hot-data MB", "query us", "entity", "gnn", "reason"
	);
	for (mode, line) in &lines {
		let cold: u64 = field(line, "cold").parse().unwrap();
		let hot: u64 = field(line, "hot").parse().unwrap();
		let peak: u64 = field(line, "peak").parse().unwrap();
		let tag = format!("{mode}/{}", field(line, "entity_variant"));
		println!(
			"{:<19} {:>12.1} {:>12.1} {:>12.1} {:>14.1} {:>11} {:>10} {:>10} {:>10}",
			tag,
			mb(cold),
			mb(hot),
			mb(peak),
			mb(hot.saturating_sub(base)),
			field(line, "search_us"),
			field(line, "entity_idx"),
			field(line, "gnn_idx"),
			field(line, "reason_idx"),
		);
	}
	println!();
}
