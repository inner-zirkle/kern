// The scaling instrument behind ROADMAP item 28, in the shape of
// `tests/gc_scale.rs`. Ignored by default: it trains a real GCN over kerns of up
// to a few thousand entities at the production 384-dim / 24-epoch settings,
// which is minutes in release and effectively unbounded in debug.
//
//   cargo test --release --test gnn_scale -- --ignored --nocapture
//
// Three questions, one test each:
//   1. what does one `GnnPropagate` cost as the kern grows (`gnn_train_scale`)
//   2. which part of it is the cost (`gnn_cost_breakdown`)
//   3. what waits behind it on the single tick loop (`tick_head_of_line_delay`)
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use base::base_types::{Entity, EntityKind, Kern, Reason};
use config::HeatConfig;
use config::TickConfig;
use gnn::gnn::propagate::GnnConfig;
use graph::graph::GraphGnn;
use graph::reason::add_reason;
use tick::tick_queue::{task, task_commit_access, Queue, TaskKind};
use tick_loop::tick_gnn_propagate::do_gnn_propagate;

const DIM: usize = 384;

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

// Ingest gives each accepted entity at most one similarity edge to its top-1
// neighbour (`add_similarity_reason`, `src/base/accept.rs:378-411`), so the
// real graph is sparse: degree ~2. `deg` is the knob because the adjacency used
// to be a dense N x N matrix insensitive to it; since 2026-07-22 it is stored as
// its nonzeros, so `deg` is now what the propagation actually scales in.
fn kern_with(n: usize, deg: usize) -> Kern {
	let mut k = Kern::new("kx", "");
	k.graviton_text = "named".into();
	k.graviton_vec = dense_vec(0);
	for i in 0..n {
		let id = format!("e{i:07}");
		k.entities.insert(
			id.clone(),
			Entity {
				id,
				vector: dense_vec(i + 1).into(),
				kind: EntityKind::Claim,
				..Default::default()
			},
		);
	}
	for i in 0..n {
		for d in 1..=deg.max(1) {
			let j = (i + d) % n;
			if i == j {
				continue;
			}
			let (from, to) = (format!("e{i:07}"), format!("e{j:07}"));
			add_reason(
				&mut k,
				Reason {
					id: format!("{from}->{to}"),
					from,
					to,
					..Default::default()
				},
			);
		}
	}
	k
}

fn graph_with(n: usize, deg: usize) -> Arc<RwLock<GraphGnn>> {
	let mut g = GraphGnn::new();
	let k = kern_with(n, deg);
	let ids: Vec<String> = k.entities.keys().cloned().collect();
	g.kerns.insert("kx".into(), k);
	// `commit_access_ids` resolves through `kern_of_entity`, so an unindexed
	// entity makes the CommitAccess probe a silent no-op.
	for id in &ids {
		g.index_entity(id, "kx");
	}
	Arc::new(RwLock::new(g))
}

#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn gnn_train_scale() {
	let cfg = GnnConfig::defaults();
	println!(
		"dim={DIM} min_thoughts={} train_epochs={}",
		cfg.min_thoughts, cfg.train_epochs
	);
	for n in [128usize, 256, 512, 1024, 2048, 4096] {
		let g = graph_with(n, 2);
		let q = Queue::new(512);
		let t = Instant::now();
		do_gnn_propagate(&q, &g, "kx", &cfg);
		let ms = t.elapsed().as_secs_f64() * 1000.0;
		let updated = g
			.read()
			.kerns
			.get("kx")
			.map(|k| {
				k.entities
					.values()
					.filter(|e| !e.gnn_vector.is_empty())
					.count()
			})
			.unwrap_or(0);
		let adj_mb = {
			let k = &g.read().kerns["kx"];
			let snap = tick_loop::tick_gnn_propagate::build_gnn_snapshot(k, &cfg);
			snap
				.map(|s| s.graph.normalized_adjacency_sparse().nnz())
				.unwrap_or(0) as f64
				* 16.0
				/ 1.0e6
		};
		println!(
			"N={n:<6} deg=2  propagate={ms:10.1}ms  ({:6.2}s)  updated={updated}  \
			 adjacency={adj_mb:.2}MB sparse (dense would be {:.1}MB)",
			ms / 1000.0,
			(n * n * 8) as f64 / 1.0e6
		);
	}
}

// Where the propagation's time goes, dense against sparse. The item blames
// `normalized_adjacency` *materialising* a dense N x N Tensor, but materialising,
// multiplying and transposing are three different costs and only one of them can
// be the dominant term. Both adjacency forms are timed per call and attributed by
// the call counts the code fixes: `train_epochs + 1` forwards x 2 GCN layers, and
// `train_epochs` backwards x 2. `full` runs the shipping path.
#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn gnn_cost_breakdown() {
	let cfg = GnnConfig::defaults();
	let hidden = (DIM / 2).clamp(16, 256);
	let e = cfg.train_epochs as f64;
	// forwards: (E+1) x [layer1 wide, layer2 narrow]; backwards: E x the same two.
	let builds = 2.0 * (e + 1.0);
	let matmuls = (e + 1.0) + e;
	let transposes = 2.0 * e;

	for n in [1024usize, 2048, 4096] {
		let k = kern_with(n, 2);
		let snap =
			tick_loop::tick_gnn_propagate::build_gnn_snapshot(&k, &cfg).expect("snapshot builds");
		let narrow = gnn::gnn::tensor::Tensor::zeros(n, hidden);

		let t = Instant::now();
		let dense = snap.graph.normalized_adjacency();
		let d_build = t.elapsed().as_secs_f64() * 1000.0;
		let t = Instant::now();
		let _ = dense.matmul(&snap.features).unwrap();
		let _ = dense.matmul(&narrow).unwrap();
		let d_mm = t.elapsed().as_secs_f64() * 1000.0;
		let t = Instant::now();
		let _ = dense.transpose();
		let d_tr = t.elapsed().as_secs_f64() * 1000.0;
		drop(dense);

		let t = Instant::now();
		let sparse = snap.graph.normalized_adjacency_sparse();
		let s_build = t.elapsed().as_secs_f64() * 1000.0;
		let t = Instant::now();
		let _ = sparse.matmul(&snap.features).unwrap();
		let _ = sparse.matmul(&narrow).unwrap();
		let s_mm = t.elapsed().as_secs_f64() * 1000.0;
		let t = Instant::now();
		let _ = sparse.transpose();
		let s_tr = t.elapsed().as_secs_f64() * 1000.0;

		let d_total = d_build * builds + d_mm * matmuls + d_tr * transposes;
		let s_total = s_build * builds + s_mm * matmuls + s_tr * transposes;

		let t = Instant::now();
		let full = gnn::gnn::propagate::run_learned_propagation(&snap, &cfg).expect("propagation");
		let full_ms = t.elapsed().as_secs_f64() * 1000.0;
		assert!(!full.updates.is_empty());

		println!(
			"N={n:<6} edges={:<7} nnz={:<8} full={full_ms:10.1}ms  \
			 non_adjacency={:.1}ms\n  \
			 build      dense 1x{d_build:8.2}ms  sparse 1x{s_build:8.3}ms   x{builds:.0}\n  \
			 matmul     dense 1x{d_mm:8.2}ms  sparse 1x{s_mm:8.3}ms   x{matmuls:.0}\n  \
			 transpose  dense 1x{d_tr:8.2}ms  sparse 1x{s_tr:8.3}ms   x{transposes:.0}\n  \
			 ATTRIBUTED dense  {d_total:10.1}ms  sparse  {s_total:9.1}ms   \
			 ({:.1}% of a dense propagation was adjacency)",
			snap.graph.edges.len(),
			sparse.nnz(),
			full_ms - s_total,
			100.0 * d_total / (full_ms - s_total + d_total),
		);
	}
}

// Every other maintenance task, for scale: what the tick loop is being asked to
// stall. Sizes are the same N so the numbers are directly comparable.
#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn other_tick_tasks_scale() {
	for n in [1024usize, 4096] {
		let g = graph_with(n, 2);
		let q = Queue::new(512);

		let t = Instant::now();
		tick::tick_stigmergy::run_gc(&g, "kx", &HeatConfig::default());
		let gc_ms = t.elapsed().as_secs_f64() * 1000.0;

		let ids: Vec<String> = g.read().kerns["kx"].entities.keys().cloned().collect();
		let commit = task_commit_access(&ids[..1.min(ids.len())]);
		let t = Instant::now();
		tick_loop::tick_tasks::do_commit_access(&g, &commit.extra, &HeatConfig::default());
		let commit_ms = t.elapsed().as_secs_f64() * 1000.0;

		let t = Instant::now();
		tick_loop::tick_idle::run_idle_sweep(&g, Duration::from_secs(3600));
		let idle_ms = t.elapsed().as_secs_f64() * 1000.0;

		let _ = &q;
		println!(
			"N={n:<6} stigmergy_gc={gc_ms:8.3}ms  commit_access={commit_ms:8.3}ms  \
			 idle_sweep={idle_ms:8.3}ms"
		);
	}
}

// The head-of-line claim itself, on the REAL loop (`tick_loop::start`), not a
// hand-rolled drain: a `GnnPropagate` for a large kern is enqueued first and a
// `CommitAccess` — the recall path's heat write-back, `src/mcp/tools_query.rs:196`
// — immediately behind it. The delay is wall time from enqueue to the access
// landing on the entity.
#[test]
#[ignore = "minutes in release; run explicitly with --ignored"]
fn tick_head_of_line_delay() {
	let rt = tokio::runtime::Runtime::new().unwrap();
	for n in [512usize, 2048] {
		for with_gnn in [false, true] {
			let g = graph_with(n, 2);
			let q = Arc::new(Queue::new(512));
			let ctx = tick_loop::TickContext {
				llm: None,
				embed: None,
				gnn_cfg: GnnConfig::defaults(),
				tick_cfg: TickConfig::default(),
				heat_cfg: HeatConfig::default(),
			};
			let _guard = rt.enter();
			let handle = tick_loop::start(q.clone(), g.clone(), ctx);

			let probe = "e0000000".to_string();
			let t = Instant::now();
			if with_gnn {
				assert!(q.enqueue(task(TaskKind::GnnPropagate, "kx")));
			}
			assert!(q.enqueue(task_commit_access(std::slice::from_ref(&probe))));

			let landed = rt.block_on(async {
				for _ in 0..600_000 {
					if g.read().kerns["kx"].entities[&probe].accessed_at.is_some() {
						return true;
					}
					tokio::time::sleep(Duration::from_millis(1)).await;
				}
				false
			});
			assert!(
				landed,
				"CommitAccess never landed; the probe is not measuring"
			);
			let ms = t.elapsed().as_secs_f64() * 1000.0;
			handle.abort();
			let label = if with_gnn { "gnn ahead" } else { "no gnn   " };
			println!("N={n:<6} {label}  commit_access landed after {ms:10.1}ms");
		}
	}
}
