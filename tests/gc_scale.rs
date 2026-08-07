// The scaling instrument behind ROADMAP item 27's two remaining bullets, in the
// shape of `tests/seed_scale.rs`. Ignored by default: it builds 100k-entity
// kerns and fills a 50k-row cold tier, which is minutes in release and
// effectively unbounded in debug.
//
//   cargo test --release --test gc_scale -- --ignored --nocapture
//
// Method for the sweep: `run_gc` returns immediately once the victim list is
// empty, so a sweep over a kern with ZERO victims is the selection scan and
// nothing else. Subtracting it from a sweep with V victims separates selection
// from eviction without touching the code under measurement.
use base::base_constants::COLD_GC_AGE;
use base::base_types::{ChunkPart, ChunkPartKind, Entity, EntityKind, Kern};
use config::HeatConfig;
use graph::graph::GraphGnn;
use tick::tick_stigmergy::run_gc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use store::base_store::Store;

const DIM: usize = 384;

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

fn entity(i: usize, victim: bool) -> Entity {
	let now = SystemTime::now();
	let old = now - (COLD_GC_AGE + Duration::from_secs(60));
	Entity {
		id: format!("e{i:07}"),
		vector: sparse_vec(i).into(),
		kind: if i.is_multiple_of(5) {
			EntityKind::Fact
		} else {
			EntityKind::Claim
		},
		chunks: vec![ChunkPart {
			kind: ChunkPartKind::Context,
			text: format!("entity {i} carries a context chunk of realistic length"),
			index: 0,
		}],
		heat: if victim { 0.0 } else { 1.0 },
		heat_updated_at: Some(if victim { old } else { now }),
		accessed_at: Some(if victim { old } else { now }),
		created_at: Some(old),
		..Default::default()
	}
}

// Victims are Claims only: Facts are GC-immune while Active, so making them
// stale would silently shrink the victim set and understate eviction cost.
fn graph(n: usize, victim_pct: usize) -> GraphGnn {
	let mut g = GraphGnn::new();
	let mut k = Kern::new("kx", "");
	for i in 0..n {
		let claim = !i.is_multiple_of(5);
		let victim = claim && (i * 100 / n.max(1)) % 100 < victim_pct;
		let e = entity(i, victim);
		k.entities.insert(e.id.clone(), e);
	}
	g.kerns.insert("kx".into(), k);
	g
}

fn victims_in(g: &GraphGnn) -> usize {
	g.kerns.values().map(|k| k.entities.len()).sum()
}

fn sweep(n: usize, victim_pct: usize, store: Option<Arc<Store>>) -> (f64, usize) {
	let mut g = graph(n, victim_pct);
	if let Some(s) = store {
		g.set_store(s);
	}
	let before = victims_in(&g);
	let g = Arc::new(parking_lot::RwLock::new(g));
	let t = Instant::now();
	run_gc(&g, "kx", &HeatConfig::default());
	let ms = t.elapsed().as_secs_f64() * 1000.0;
	let after = victims_in(&g.read());
	(ms, before - after)
}

#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn gc_sweep_scale() {
	for n in [10_000usize, 100_000] {
		// Pure selection: no victim clears the gates, so run_gc scans and returns.
		const REPS: usize = 20;
		let mut scan_ms = 0.0;
		for _ in 0..REPS {
			scan_ms += sweep(n, 0, None).0;
		}
		scan_ms /= REPS as f64;
		println!("N={n:<7} victims=  0%  selection_scan={scan_ms:9.3}ms  (whole sweep)");

		for pct in [1usize, 10, 100] {
			let dir = tempfile::tempdir().unwrap();
			let store = Arc::new(Store::open(&dir.path().to_string_lossy()).unwrap());
			let (ms, evicted) = sweep(n, pct, Some(store));
			let evict_ms = ms - scan_ms;
			println!(
				"N={n:<7} victims={pct:>3}%  sweep={ms:9.3}ms  selection={scan_ms:9.3}ms \
				 ({:5.1}% of sweep)  evict+spill={evict_ms:9.3}ms  evicted={evicted}",
				100.0 * scan_ms / ms
			);
		}
	}
}

#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn cold_search_scale() {
	for rows in [1_000usize, 10_000, 50_000] {
		let dir = tempfile::tempdir().unwrap();
		let store = Store::open(&dir.path().to_string_lossy()).unwrap();
		for chunk in (0..rows).collect::<Vec<_>>().chunks(2_000) {
			let batch: Vec<Entity> = chunk.iter().map(|i| entity(*i, true)).collect();
			store.cold_put_all(&batch).unwrap();
		}
		let q = sparse_vec(12345);
		for _ in 0..2 {
			std::hint::black_box(store.cold_search(&q, 10).unwrap());
		}
		const REPS: usize = 10;
		let t = Instant::now();
		let mut hits = 0;
		for _ in 0..REPS {
			let h = store.cold_search(&q, 10).unwrap();
			hits = h.len();
			std::hint::black_box(h);
		}
		let ms = t.elapsed().as_secs_f64() * 1000.0 / REPS as f64;
		// `cold_all` is the full-table Entity decode with no scoring — what
		// `cold_search` used to pay per query before scoring moved to the vector
		// side table, and the ceiling any change here is measured against.
		let t = Instant::now();
		for _ in 0..REPS {
			std::hint::black_box(store.cold_all().unwrap());
		}
		let decode_ms = t.elapsed().as_secs_f64() * 1000.0 / REPS as f64;
		println!(
			"cold rows={rows:<7} cold_search(k=10)={ms:9.3}ms hits={hits}  \
			 full_tier_decode={decode_ms:9.3}ms ({:5.1}x the search)",
			decode_ms / ms
		);
	}
}

// An embedding model returns a dense vector; `sparse_vec` is 7 non-zero
// components in 384, which zstd crushes to nothing. Disk cost is reported for
// both because the answer differs by more than an order of magnitude and only
// the dense one describes a real deployment.
fn dense_vec(seed: usize) -> Vec<f32> {
	let mut h = seed as u64 | 1;
	let mut v: Vec<f32> = (0..DIM)
		.map(|_| {
			h ^= h << 13;
			h ^= h >> 7;
			h ^= h << 17;
			(h % 2_000_000) as f32 / 1_000_000.0 - 1.0
		})
		.collect();
	let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
	for x in &mut v {
		*x /= n;
	}
	v
}

// Isolates the transaction boundary and nothing else: `cold_spill` and
// `cold_put_all` encode the same rows and issue the same two `put`s per row —
// they differ only in where `write_txn`/`commit` sit. Whatever separates these
// two columns IS the per-victim commit.
#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn cold_spill_per_victim_vs_batched() {
	for victims in [100usize, 800, 5_000, 20_000] {
		let batch: Vec<Entity> = (0..victims)
			.map(|i| {
				let mut e = entity(i, true);
				e.vector = dense_vec(i).into();
				e
			})
			.collect();

		let d1 = tempfile::tempdir().unwrap();
		let s1 = Store::open(&d1.path().to_string_lossy()).unwrap();
		let t = Instant::now();
		for e in &batch {
			s1.cold_spill(e).unwrap();
		}
		let per_ms = t.elapsed().as_secs_f64() * 1000.0;

		let d2 = tempfile::tempdir().unwrap();
		let s2 = Store::open(&d2.path().to_string_lossy()).unwrap();
		let t = Instant::now();
		s2.cold_put_all(&batch).unwrap();
		let batch_ms = t.elapsed().as_secs_f64() * 1000.0;

		assert_eq!(
			s1.cold_all().unwrap().len(),
			s2.cold_all().unwrap().len(),
			"both paths must land the same rows or the comparison is meaningless"
		);
		println!(
			"victims={victims:<7} per_victim={per_ms:10.3}ms ({:7.4}ms each)  \
			 one_txn={batch_ms:9.3}ms ({:7.4}ms each)  speedup={:6.1}x",
			per_ms / victims as f64,
			batch_ms / victims as f64,
			per_ms / batch_ms
		);
	}
}

// The write side of the same tier. `run_gc` spills one victim at a time, so the
// per-spill cost is what a sweep multiplies by V — and it is what any change
// that adds a second write to the tier has to be paid for out of.
#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn cold_spill_scale() {
	for dense in [false, true] {
		for spills in [1_000usize, 5_000] {
			let dir = tempfile::tempdir().unwrap();
			let store = Store::open(&dir.path().to_string_lossy()).unwrap();
			let batch: Vec<Entity> = (0..spills)
				.map(|i| {
					let mut e = entity(i, true);
					if dense {
						e.vector = dense_vec(i).into();
					}
					e
				})
				.collect();
			let t = Instant::now();
			for e in &batch {
				store.cold_spill(e).unwrap();
			}
			let total = t.elapsed().as_secs_f64() * 1000.0;
			let shape = if dense { "dense" } else { "sparse" };
			println!(
				"{shape:<6} spills={spills:<7} total={total:10.3}ms  per_spill={:7.4}ms  \
				 bytes_on_disk={}",
				total / spills as f64,
				store.data_file_len()
			);
		}
	}
}
