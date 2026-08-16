//! The background loop. [`start`] spawns the consumer that drains the task
//! [`Queue`] — clustering, naming, enrichment, question seeding, GNN
//! propagation, GC — one task at a time against the shared graph; [`tick_sync`]
//! runs the same drain inline for tests and one-shot commands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use base::base_constants::{KERN_COHESION_THRESHOLD, KERN_MIN_CLUSTER_SIZE};
use config::HeatConfig;
use config::TickConfig;
use gnn::gnn::propagate::GnnConfig;
use graph::graph::GraphGnn;

use crate::tick_cluster::{cohesion, is_core_cluster, vector_cluster, Cluster};
use crate::tick_gnn_propagate::do_gnn_propagate;
use crate::tick_tasks::{
	do_classify_contradiction, do_commit_access, do_disk_consolidate, do_enrich, do_name, do_persist,
	do_reembed, do_resolve, do_seed_questions, EmbedFunc, LlmFunc,
};
use tick::tick_queue::{task, task_extra, Queue, Task, TaskKind};

pub struct TickContext {
	pub llm: Option<LlmFunc>,
	pub embed: Option<EmbedFunc>,
	pub gnn_cfg: GnnConfig,
	pub tick_cfg: TickConfig,
	pub heat_cfg: HeatConfig,
}

pub fn start(
	q: Arc<Queue>,
	g: Arc<RwLock<GraphGnn>>,
	ctx: TickContext,
) -> tokio::task::JoinHandle<()> {
	let mut rx = q.take_receiver().expect("receiver already taken");
	tokio::spawn(async move {
		// Owned by the loop: aborting the tick handle drops the sender, which ends
		// the trainer thread rather than leaving it holding this store's graph.
		let trainer = gnn_trainer(&q, &g, &ctx);
		while let Some(t) = rx.recv().await {
			// Drain Rephrase edges re-pointed at a supersede and re-enqueue their
			// classification against the new active entity (ROADMAP item 60).
			for (kern_id, rid) in g.read().drain_pending_reclass() {
				q.enqueue(task_extra(TaskKind::ClassifyContradiction, &kern_id, &rid));
			}
			q.dequeued(&t);
			run_guarded(&q, &t, || process_task(&q, &g, &t, &ctx, Some(&trainer)));
		}
	})
}

fn gnn_trainer(
	q: &Arc<Queue>,
	g: &Arc<RwLock<GraphGnn>>,
	ctx: &TickContext,
) -> crate::tick_trainer::Trainer {
	let (tq, tg, cfg) = (q.clone(), g.clone(), ctx.gnn_cfg);
	crate::tick_trainer::Trainer::spawn(q.clone(), move |kern_id| {
		do_gnn_propagate(&tq, &tg, kern_id, &cfg)
	})
}

// A panicking task must cost one task, not every future tick. `AssertUnwindSafe` is
// deliberate: the graph lock does not poison, so the loop resumes over state the dead
// task may have half-written — which is exactly what the error line reports.
fn run_guarded(q: &Queue, t: &Task, run: impl FnOnce()) {
	let started = Instant::now();
	match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
		// `task_avg_ms` answers "how long does maintenance take"; feeding it the
		// duration of work that never finished makes it lie as failures climb.
		Ok(()) => q.record_task_latency(started.elapsed()),
		Err(payload) => {
			let message = panic_message(payload.as_ref());
			tracing::error!(
				target: "kern.tick",
				kind = ?t.kind,
				kern = %t.kern_id,
				panic = %message,
				"tick task panicked; maintenance continues but this kern's graph state may be partially written"
			);
			q.record_task_panic(t, &message);
		}
	}
	q.done();
}

pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
	if let Some(s) = payload.downcast_ref::<&str>() {
		(*s).to_string()
	} else if let Some(s) = payload.downcast_ref::<String>() {
		s.clone()
	} else {
		"unknown panic payload".to_string()
	}
}

// `trainer` is `None` only on the synchronous drain (`tick_sync`), whose contract
// is that the work is done when it returns; there the propagation runs inline.
fn process_task(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	t: &Task,
	ctx: &TickContext,
	trainer: Option<&crate::tick_trainer::Trainer>,
) {
	let (llm, embed) = (ctx.llm.as_ref(), ctx.embed.as_ref());
	match t.kind {
		TaskKind::Cluster => do_cluster(q, g, &t.kern_id, &ctx.tick_cfg, llm, embed),
		TaskKind::SeedQuestions => do_seed_questions(q, g, &t.extra, llm),
		TaskKind::ClassifyContradiction => {
			do_classify_contradiction(q, g, &t.kern_id, &t.extra, llm, embed)
		}
		TaskKind::Name => do_name(q, g, &t.kern_id, &ctx.tick_cfg, llm, embed),
		TaskKind::Enrich => do_enrich(q, g, &t.kern_id, &t.extra, llm, embed),
		TaskKind::ResolveQuestion => do_resolve(q, g, &t.kern_id, &t.extra),
		TaskKind::Persist => do_persist(g, &t.kern_id),
		TaskKind::GnnPropagate => match trainer {
			Some(tr) => {
				tr.submit(&t.kern_id);
			}
			None => do_gnn_propagate(q, g, &t.kern_id, &ctx.gnn_cfg),
		},
		TaskKind::StigmergyGc => tick::tick_stigmergy::run_gc(g, &t.kern_id, &ctx.heat_cfg),
		TaskKind::Reembed => do_reembed(g, &t.kern_id, embed),
		TaskKind::DiskConsolidate => do_disk_consolidate(g),
		TaskKind::IdleSweep => {
			crate::tick_idle::run_idle_sweep(g, Duration::from_secs(ctx.tick_cfg.kern_idle_timeout_secs));
		}
		TaskKind::CommitAccess => do_commit_access(g, &t.extra, &ctx.heat_cfg),
	}
}

fn do_cluster(
	q: &Queue,
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	tick_cfg: &TickConfig,
	llm: Option<&LlmFunc>,
	_embed: Option<&EmbedFunc>,
) {
	let mut graph = g.write();

	let (clusters, spawn_indices) = match graph.kerns.get(kern_id) {
		Some(kern) => select_spawn_clusters(kern, tick_cfg.max_cluster_sample),
		None => return,
	};

	let spawned_children = spawn_child_clusters(&mut graph, kern_id, &clusters, &spawn_indices);

	let (enrich_jobs, question_jobs) = match graph.kerns.get(kern_id) {
		Some(kern) => collect_follow_up_jobs(kern),
		None => {
			drop(graph);
			return;
		}
	};

	let evicted = evict_empty_children(&mut graph, kern_id);

	let is_unnamed = graph
		.kerns
		.get(kern_id)
		.map(|k| k.is_unnamed())
		.unwrap_or(false);

	drop(graph);

	if is_unnamed && llm.is_some() {
		q.enqueue(task(TaskKind::Name, kern_id));
	}
	for child_id in &spawned_children {
		q.enqueue(task(TaskKind::Cluster, child_id));
	}
	for rid in &enrich_jobs {
		q.enqueue(task_extra(TaskKind::Enrich, kern_id, rid));
	}
	for rid in &question_jobs {
		q.enqueue(task_extra(TaskKind::ResolveQuestion, kern_id, rid));
	}
	let did_structural_work =
		!spawned_children.is_empty() || evicted || !enrich_jobs.is_empty() || !question_jobs.is_empty();
	if !spawned_children.is_empty() || evicted {
		// Persist children BEFORE the parent: parent-first + crash erases the
		// migrated entities from disk; child-first merely duplicates them briefly.
		for child_id in &spawned_children {
			q.enqueue(task(TaskKind::Persist, child_id));
		}
		q.enqueue(task(TaskKind::Persist, kern_id));
	}
	// No structural change -> previous gnn_vector state still valid; skip GNN.
	if did_structural_work {
		q.enqueue(task(TaskKind::GnnPropagate, kern_id));
	}
}

fn select_spawn_clusters(
	kern: &base::base_types::Kern,
	max_sample: usize,
) -> (Vec<Cluster>, Vec<usize>) {
	// UNNAMED KERNS NEVER SPAWN — else each pass descends one level unboundedly
	// (see select_spawn_clusters_never_spawns_from_an_unnamed_kern).
	if !kern.is_named() {
		return (Vec::new(), Vec::new());
	}

	let entities: Vec<_> = kern.entities.values().collect();
	let clusters = vector_cluster(&entities, max_sample);

	let mut spawn_indices = Vec::new();
	for (i, c) in clusters.iter().enumerate() {
		if is_core_cluster(c, &kern.graviton_vec) {
			continue;
		}
		if c.members.len() >= KERN_MIN_CLUSTER_SIZE && cohesion(&c.members) >= KERN_COHESION_THRESHOLD {
			spawn_indices.push(i);
		}
	}
	(clusters, spawn_indices)
}

fn spawn_child_clusters(
	graph: &mut GraphGnn,
	kern_id: &str,
	clusters: &[Cluster],
	spawn_indices: &[usize],
) -> Vec<String> {
	let mut spawned_children = Vec::new();
	for i in spawn_indices {
		// One DISTINCT child per cluster: never `get_or_spawn_unnamed_child` — it
		// reuses the first unnamed child, collapsing every cluster into one kern.
		let child_id = graph::accept::spawn_unnamed_child(graph, kern_id);
		for m in &clusters[*i].members {
			// Carries outgoing reasons and reindexes; a rejected move leaves the entity put.
			if let Err(e) = graph::reason::move_entity(graph, kern_id, &child_id, &m.id) {
				tracing::warn!(
					target: "kern.cluster",
					from = %kern_id,
					to = %child_id,
					entity = %m.id,
					error = %e,
					"cluster migration skipped"
				);
			}
		}
		spawned_children.push(child_id);
	}
	spawned_children
}

fn collect_follow_up_jobs(kern: &base::base_types::Kern) -> (Vec<String>, Vec<String>) {
	use base::base_types::ReasonKind;

	let mut enrich_jobs = Vec::new();
	for r in kern.reasons.values() {
		if r.is_enriched() || r.kind == ReasonKind::Spawn || r.kind == ReasonKind::Question {
			continue;
		}
		if !kern.entities.contains_key(&r.from) || !kern.entities.contains_key(&r.to) {
			continue;
		}
		enrich_jobs.push(r.id.clone());
	}

	let mut question_jobs = Vec::new();
	for r in kern.reasons.values() {
		if r.kind == ReasonKind::Question && r.to.is_empty() {
			question_jobs.push(r.id.clone());
		}
	}
	(enrich_jobs, question_jobs)
}

fn evict_empty_children(graph: &mut GraphGnn, kern_id: &str) -> bool {
	let children_ids = match graph.kerns.get(kern_id) {
		Some(k) => k.children.clone(),
		None => return false,
	};

	let mut alive = Vec::new();
	let mut evicted = false;
	for child_id in &children_ids {
		// An unloaded child is resident on disk, not dead. Treating the map
		// miss as "does not exist" deregistered it — and deregister deletes
		// the disk row, so an idle-unloaded kern full of entities was erased.
		if graph.is_unloaded(child_id) {
			alive.push(child_id.clone());
			continue;
		}
		let (named, has_thoughts, exists) = match graph.kerns.get(child_id) {
			Some(c) => (c.is_named(), !c.entities.is_empty(), true),
			None => (false, false, false),
		};
		if !exists || (!named && !has_thoughts) {
			if exists {
				let stray_ids: Vec<String> = graph
					.kerns
					.get(child_id)
					.map(|c| c.entities.keys().cloned().collect())
					.unwrap_or_default();
				for tid in stray_ids {
					let t = graph
						.kerns
						.get_mut(child_id)
						.and_then(|c| c.entities.remove(&tid));
					if let Some(t) = t {
						if let Some(parent) = graph.kerns.get_mut(kern_id) {
							parent.entities.insert(tid, t);
						}
					}
				}
			}
			graph.deregister(child_id);
			evicted = true;
			continue;
		}
		alive.push(child_id.clone());
	}
	if let Some(kern) = graph.kerns.get_mut(kern_id) {
		kern.children = alive;
	}
	evicted
}

pub fn enqueue_all(q: &Queue, g: &Arc<RwLock<GraphGnn>>) {
	let graph = g.read();
	for kern in graph.all() {
		if !kern.entities.is_empty() {
			q.enqueue(task(TaskKind::Cluster, &kern.id));
		}
	}
}

pub fn tick_sync(
	g: &Arc<RwLock<GraphGnn>>,
	kern_id: &str,
	llm: Option<&LlmFunc>,
	embed: Option<&EmbedFunc>,
) {
	let q = Queue::new(256);
	q.enqueue(task(TaskKind::Cluster, kern_id));

	let ctx = TickContext {
		llm: llm.cloned(),
		embed: embed.cloned(),
		gnn_cfg: GnnConfig::defaults(),
		tick_cfg: TickConfig::default(),
		heat_cfg: HeatConfig::default(),
	};

	let gg = Arc::clone(g);
	let mut rx = q.take_receiver().unwrap();
	while let Ok(t) = rx.try_recv() {
		q.dequeued(&t);
		process_task(&q, &gg, &t, &ctx, None);
		q.done();
	}
}

#[cfg(test)]
#[path = "tests/tick_test.rs"]
mod tick_tests;
