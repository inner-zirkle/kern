// The instrument behind ROADMAP item 32: does an entity's survival depend on its
// depth in the kern tree, holding usage constant? Ignored by default — it runs
// the lifecycle forward over hundreds of simulated days at the real 60s tick
// cadence and the real `relaxed` half-life, which is minutes in debug.
//
//   cargo test --release --test depth_bias -- --ignored --nocapture
//
// Simulated time, not slept time: `rewind` moves every entity's stamps backwards
// by one tick, which is exactly equivalent to moving the wall clock forwards and
// leaves the code under measurement (`pulse`, `commit_access_ids`,
// `run_gc`) untouched. Two cohorts sit at every depth with IDENTICAL usage —
// `used` re-accessed every 6h through the whole run, `unused` accessed once at
// t=0 — so any survival difference between depths is depth, and nothing else.
use base::base_constants::{COLD_GC_AGE, COLD_HEAT_THRESHOLD, PULSE_DECAY, PULSE_THRESHOLD};
use base::base_types::{Entity, EntityKind, Kern};
use config::HeatConfig;
use graph::graph::GraphGnn;
use parking_lot::RwLock;
use retrieval::score::commit_access_ids;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tick::tick_pulse::pulse;
use tick::tick_queue::Queue;
use tick::tick_stigmergy::run_gc;

const TICK: Duration = Duration::from_secs(60);
const GC_EVERY_TICKS: usize = 60;
const ACCESS_EVERY_TICKS: usize = 6 * 60;
const DEPTHS: usize = 8;

fn kern_id(d: usize) -> String {
	format!("k{d}")
}

fn entity(id: &str, now: SystemTime) -> Entity {
	Entity {
		id: id.into(),
		kind: EntityKind::Claim,
		// One access at t=0 — the same starting heat and the same stamps at every
		// depth, so the cohorts differ in position only.
		heat: HeatConfig::default().deposit_access,
		heat_updated_at: Some(now),
		accessed_at: Some(now),
		created_at: Some(now),
		..Default::default()
	}
}

// A chain root -> k1 -> ... -> k{DEPTHS-1}: deeper than the pulse can reach, so
// the reach boundary falls inside the graph instead of past its bottom.
fn chain() -> GraphGnn {
	let now = SystemTime::now();
	let mut g = GraphGnn::new();
	for d in 0..DEPTHS {
		let mut k = Kern::new(
			kern_id(d),
			if d == 0 {
				String::new()
			} else {
				kern_id(d - 1)
			},
		);
		if d + 1 < DEPTHS {
			k.children = vec![kern_id(d + 1)];
		}
		for e in [
			entity(&format!("used{d}"), now),
			entity(&format!("unused{d}"), now),
		] {
			k.entities.insert(e.id.clone(), e);
		}
		g.register(k);
	}
	g
}

fn rewind(g: &mut GraphGnn, by: Duration) {
	for k in g.kerns.values_mut() {
		for e in k.entities.values_mut() {
			for v in [
				&mut e.heat_updated_at,
				&mut e.accessed_at,
				&mut e.created_at,
			]
			.into_iter()
			.flatten()
			{
				*v -= by;
			}
		}
	}
}

struct Fate {
	depth: usize,
	used_died_day: Option<f64>,
	unused_died_day: Option<f64>,
	unused_heat: f32,
}

fn run(days: f64, cfg: &HeatConfig) -> Vec<Fate> {
	let ticks = (days * 86_400.0 / TICK.as_secs_f64()) as usize;
	let graph = Arc::new(RwLock::new(chain()));
	let q = Queue::new(4096);
	let used_ids: Vec<String> = (0..DEPTHS).map(|d| format!("used{d}")).collect();
	let mut died: Vec<(Option<usize>, Option<usize>)> = vec![(None, None); DEPTHS];

	for tick in 0..ticks {
		{
			let mut g = graph.write();
			rewind(&mut g, TICK);
			pulse(&q, &g, &kern_id(0), 1.0);
			if tick % ACCESS_EVERY_TICKS == 0 {
				commit_access_ids(&mut g, &used_ids, cfg);
			}
		}
		if tick % GC_EVERY_TICKS == GC_EVERY_TICKS - 1 {
			for d in 0..DEPTHS {
				run_gc(&graph, &kern_id(d), cfg);
			}
			let g = graph.read();
			for (d, died) in died.iter_mut().enumerate() {
				let k = g.kerns.get(&kern_id(d)).expect("kern resident");
				if died.0.is_none() && !k.entities.contains_key(&format!("used{d}")) {
					died.0 = Some(tick);
				}
				if died.1.is_none() && !k.entities.contains_key(&format!("unused{d}")) {
					died.1 = Some(tick);
				}
			}
		}
	}

	let g = graph.read();
	let to_day = |t: Option<usize>| t.map(|t| t as f64 * TICK.as_secs_f64() / 86_400.0);
	(0..DEPTHS)
		.map(|d| Fate {
			depth: d,
			used_died_day: to_day(died[d].0),
			unused_died_day: to_day(died[d].1),
			unused_heat: g
				.kerns
				.get(&kern_id(d))
				.and_then(|k| k.entities.get(&format!("unused{d}")))
				.map(|e| e.heat)
				.unwrap_or(0.0),
		})
		.collect()
}

// What one tick of the pulse costs, over a tree whose whole pulse reach is
// populated. The number the removal is paid out of: the deposit pass was
// O(entities in reach) heat writes plus one `Vec<String>` of every entity id per
// kern, under the graph WRITE lock, every 60s.
fn populated_reach() -> GraphGnn {
	const BRANCH: usize = 4;
	const PER_KERN: usize = 300;
	let now = SystemTime::now();
	let mut g = GraphGnn::new();
	let mut level = vec!["r".to_string()];
	let mut next_id = 0usize;
	for depth in 0..5 {
		let mut next = Vec::new();
		for id in &level {
			let mut k = Kern::new(id.clone(), "");
			if depth + 1 < 5 {
				for _ in 0..BRANCH {
					next_id += 1;
					let cid = format!("k{next_id}");
					k.children.push(cid.clone());
					next.push(cid);
				}
			}
			for i in 0..PER_KERN {
				let e = entity(&format!("{id}-e{i}"), now);
				k.entities.insert(e.id.clone(), e);
			}
			g.register(k);
		}
		level = next;
	}
	g
}

#[test]
#[ignore = "builds a 100k-entity tree; run explicitly with --ignored"]
fn pulse_cost_per_tick() {
	let g = populated_reach();
	let entities: usize = g.kerns.values().map(|k| k.entities.len()).sum();
	let q = Queue::new(1 << 16);
	// Warm: the Cluster dedup marks every key pending on the first pass, so a
	// steady-state tick is what the second and later passes measure.
	for _ in 0..3 {
		pulse(&q, &g, "r", 1.0);
	}
	const REPS: usize = 50;
	let t = std::time::Instant::now();
	for _ in 0..REPS {
		pulse(&q, &g, "r", 1.0);
	}
	let us = t.elapsed().as_secs_f64() * 1e6 / REPS as f64;
	println!(
		"\npulse over {} kerns / {entities} entities in reach: {us:.1}us per tick",
		g.kerns.len()
	);
}

#[test]
#[ignore = "hundreds of simulated days; run explicitly with --ignored"]
fn depth_is_an_eviction_bias() {
	let reach = (0..)
		.take_while(|d| PULSE_DECAY.powi(*d) >= PULSE_THRESHOLD)
		.count();
	println!(
		"pulse reach: strength 1.0, decay {PULSE_DECAY}/level, floor {PULSE_THRESHOLD} \
		 -> depths 0..={} deposit, {reach} levels total",
		reach - 1
	);
	println!(
		"gates: heat >= {COLD_HEAT_THRESHOLD} spares; untouched <= {}d spares",
		COLD_GC_AGE.as_secs_f64() / 86_400.0
	);

	// Every shipped preset, not `HeatConfig::default()` — no deployment runs the
	// raw struct default. A single 1.0 access deposit needs log2(1/0.01) = 6.64
	// half-lives to fall under the cold gate, so each horizon has to clear that
	// or the unused cohort dies nowhere and the run proves nothing.
	for (preset, half_life_days) in [("relaxed", 30.0f64), ("medium", 7.0), ("tight", 3.0)] {
		let cfg = HeatConfig {
			half_life_secs: (half_life_days * 86_400.0) as u64,
			..HeatConfig::default()
		};
		let days = (half_life_days * 8.0).max(9.0);
		println!(
			"\npreset {preset}: half-life {half_life_days}d, horizon {days}d, 60s ticks, hourly \
			 GC, `used` re-accessed every 6h\ndepth  used                 unused               \
			 unused_heat"
		);
		for f in run(days, &cfg) {
			let fmt = |d: Option<f64>| match d {
				Some(d) => format!("evicted day {d:.1}"),
				None => "ALIVE".to_string(),
			};
			println!(
				"{:>5}  {:<20} {:<20} {:.4}",
				f.depth,
				fmt(f.used_died_day),
				fmt(f.unused_died_day),
				f.unused_heat
			);
		}
	}
}
