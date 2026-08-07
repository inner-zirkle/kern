//! The background loop. [`start`] spawns the consumer that drains the task
//! [`Queue`] — clustering, naming, enrichment, question seeding, GNN
//! propagation, GC — one task at a time against the shared graph; [`tick_sync`]
//! runs the same drain inline for tests and one-shot commands.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

use gnn::gnn::propagate::GnnConfig;
use base::base_constants::{KERN_COHESION_THRESHOLD, KERN_MIN_CLUSTER_SIZE};
use config::HeatConfig;
use config::TickConfig;
use graph::graph::GraphGnn;

use crate::tick_cluster::{cohesion, is_core_cluster, vector_cluster, Cluster};
use crate::tick_gnn_propagate::do_gnn_propagate;
use crate::tick_queue::{task, task_extra, Queue, Task, TaskKind};
use crate::tick_tasks::{
	do_classify_contradiction, do_commit_access, do_disk_consolidate, do_enrich, do_name, do_persist,
	do_reembed, do_resolve, do_seed_questions, BroadcastQuestionFunc, EmbedFunc, LlmFunc,
};

pub struct TickContext {
	pub llm: Option<LlmFunc>,
	pub embed: Option<EmbedFunc>,
	pub broadcast_q: Option<BroadcastQuestionFunc>,
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
		TaskKind::ResolveQuestion => do_resolve(q, g, &t.kern_id, &t.extra, ctx.broadcast_q.as_ref()),
		TaskKind::Persist => do_persist(g, &t.kern_id),
		TaskKind::GnnPropagate => match trainer {
			Some(tr) => {
				tr.submit(&t.kern_id);
			}
			None => do_gnn_propagate(q, g, &t.kern_id, &ctx.gnn_cfg),
		},
		TaskKind::StigmergyGc => crate::tick_stigmergy::run_gc(g, &t.kern_id, &ctx.heat_cfg),
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
	bq: Option<&BroadcastQuestionFunc>,
) {
	let q = Queue::new(256);
	q.enqueue(task(TaskKind::Cluster, kern_id));

	let ctx = TickContext {
		llm: llm.cloned(),
		embed: embed.cloned(),
		broadcast_q: bq.cloned(),
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
mod tests {
	use super::*;
	use base::base_types::{Entity, Kern, Reason, ReasonKind};
	use graph::reason::add_reason;

	fn parent_child(child_named: bool, child_has_thought: bool) -> (GraphGnn, String, String) {
		let mut g = GraphGnn::new();
		let (pid, cid) = ("p".to_string(), "c".to_string());
		let mut parent = Kern::new(&pid, "");
		parent.children = vec![cid.clone()];
		let mut child = Kern::new(&cid, &pid);
		if child_named {
			child.graviton_text = "named".into();
		}
		if child_has_thought {
			child.entities.insert(
				"e1".into(),
				Entity {
					id: "e1".into(),
					..Default::default()
				},
			);
		}
		g.kerns.insert(pid.clone(), parent);
		g.kerns.insert(cid.clone(), child);
		(g, pid, cid)
	}

	#[test]
	fn evict_reaps_empty_unnamed_child() {
		let (mut g, pid, cid) = parent_child(false, false);
		assert!(evict_empty_children(&mut g, &pid));
		assert!(
			!g.kerns.contains_key(&cid),
			"empty unnamed child deregistered"
		);
		assert!(
			g.kerns.get(&pid).unwrap().children.is_empty(),
			"child pruned from parent"
		);
	}

	#[test]
	fn evict_keeps_unnamed_child_that_has_thoughts() {
		let (mut g, pid, cid) = parent_child(false, true);
		assert!(!evict_empty_children(&mut g, &pid));
		assert!(g.kerns.contains_key(&cid));
		assert_eq!(g.kerns.get(&pid).unwrap().children, vec![cid]);
	}

	#[test]
	fn collect_jobs_splits_enrich_and_question_edges() {
		let mut k = Kern::new("k", "");
		k.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				..Default::default()
			},
		);
		k.entities.insert(
			"b".into(),
			Entity {
				id: "b".into(),
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				from: "a".into(),
				to: "b".into(),
				id: "a->b".into(),
				kind: ReasonKind::Similarity,
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				from: "a".into(),
				to: String::new(),
				id: "q1".into(),
				kind: ReasonKind::Question,
				..Default::default()
			},
		);

		let (enrich, questions) = collect_follow_up_jobs(&k);
		assert_eq!(
			enrich,
			vec!["a->b".to_string()],
			"only the un-enriched real edge"
		);
		assert_eq!(
			questions,
			vec!["q1".to_string()],
			"only the open question edge"
		);
	}

	#[test]
	fn collect_jobs_skips_edges_with_missing_endpoint() {
		let mut k = Kern::new("k", "");
		k.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				from: "a".into(),
				to: "b".into(),
				id: "a->b".into(),
				kind: ReasonKind::Similarity,
				..Default::default()
			},
		);
		let (enrich, questions) = collect_follow_up_jobs(&k);
		assert!(enrich.is_empty(), "edge with a missing endpoint is skipped");
		assert!(questions.is_empty());
	}

	#[test]
	fn spawn_child_migrates_cluster_members_into_new_child() {
		let mut g = GraphGnn::new();
		let pid = "p".to_string();
		let mut parent = Kern::new(&pid, "");
		parent.entities.insert(
			"a".into(),
			Entity {
				id: "a".into(),
				..Default::default()
			},
		);
		parent.entities.insert(
			"b".into(),
			Entity {
				id: "b".into(),
				..Default::default()
			},
		);
		g.kerns.insert(pid.clone(), parent);

		let clusters = vec![Cluster {
			members: vec![
				Entity {
					id: "a".into(),
					..Default::default()
				},
				Entity {
					id: "b".into(),
					..Default::default()
				},
			],
		}];
		let spawned = spawn_child_clusters(&mut g, &pid, &clusters, &[0]);

		assert_eq!(spawned.len(), 1, "one selected cluster spawns one child");
		let child_id = &spawned[0];
		assert!(
			g.kerns.get(&pid).unwrap().entities.is_empty(),
			"cluster members moved out of the parent",
		);
		let child = g.kerns.get(child_id).expect("spawned child exists");
		assert!(
			child.entities.contains_key("a") && child.entities.contains_key("b"),
			"both members landed in the new child kern",
		);
	}

	#[test]
	fn spawn_child_clusters_creates_a_distinct_child_per_cluster() {
		let mut g = GraphGnn::new();
		let pid = "p".to_string();
		let mut parent = Kern::new(&pid, "");
		for id in ["a", "b", "c", "d"] {
			parent.entities.insert(
				id.into(),
				Entity {
					id: id.into(),
					..Default::default()
				},
			);
		}
		g.kerns.insert(pid.clone(), parent);

		let clusters = vec![
			Cluster {
				members: vec![
					Entity {
						id: "a".into(),
						..Default::default()
					},
					Entity {
						id: "b".into(),
						..Default::default()
					},
				],
			},
			Cluster {
				members: vec![
					Entity {
						id: "c".into(),
						..Default::default()
					},
					Entity {
						id: "d".into(),
						..Default::default()
					},
				],
			},
		];
		let spawned = spawn_child_clusters(&mut g, &pid, &clusters, &[0, 1]);

		assert_eq!(spawned.len(), 2, "two selected clusters spawn two ids");
		assert_ne!(
			spawned[0], spawned[1],
			"each cluster lands in a DISTINCT child kern"
		);
		let c0 = g.kerns.get(&spawned[0]).expect("first child exists");
		assert!(
			c0.entities.contains_key("a") && c0.entities.contains_key("b"),
			"cluster 0 members"
		);
		assert!(
			!c0.entities.contains_key("c") && !c0.entities.contains_key("d"),
			"no cross-contamination"
		);
		let c1 = g.kerns.get(&spawned[1]).expect("second child exists");
		assert!(
			c1.entities.contains_key("c") && c1.entities.contains_key("d"),
			"cluster 1 members"
		);
		assert!(
			g.kerns.get(&pid).unwrap().entities.is_empty(),
			"all clustered members moved out of the parent",
		);
	}

	#[test]
	fn select_spawn_clusters_never_spawns_from_an_unnamed_kern() {
		let mut kern = Kern::new("k", "");
		assert!(!kern.is_named(), "precondition: kern is unnamed");
		for i in 0..base::base_constants::KERN_MIN_CLUSTER_SIZE {
			let id = format!("e{i}");
			kern.entities.insert(
				id.clone(),
				Entity {
					id,
					vector: vec![1.0, 0.0].into(),
					..Default::default()
				},
			);
		}

		let (_, spawn_indices) = select_spawn_clusters(&kern, 100);
		assert!(
			spawn_indices.is_empty(),
			"an unnamed kern must never spawn children; got {spawn_indices:?}",
		);
	}

	#[test]
	fn select_spawn_clusters_still_spawns_off_core_cluster_from_named_kern() {
		let mut kern = Kern::new("k", "");
		kern.graviton_text = "named".into();
		kern.graviton_vec = vec![1.0, 0.0];
		assert!(kern.is_named(), "precondition: kern is named");
		for i in 0..base::base_constants::KERN_MIN_CLUSTER_SIZE {
			let id = format!("e{i}");
			kern.entities.insert(
				id.clone(),
				Entity {
					id,
					vector: vec![0.0, 1.0].into(),
					..Default::default()
				},
			);
		}

		let (_, spawn_indices) = select_spawn_clusters(&kern, 100);
		assert_eq!(
			spawn_indices.len(),
			1,
			"a named kern's off-core cohesive cluster still spawns",
		);
	}

	#[test]
	fn evict_keeps_an_idle_unloaded_child_registered() {
		// Regression for the wiped-store bug: the idle sweep unloaded a child
		// (resident on disk, reloadable), evict then read the map miss as
		// "dead" and deregistered it — and deregister deletes the disk row.
		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		let root_id = g.root.id.clone();
		let store = store::base_store::Store::open(&dir.path().to_string_lossy()).unwrap();
		g.set_store(Arc::new(store));

		let mut child = Kern::new("generic", &root_id);
		child.graviton_text = "generic".into();
		child.entities.insert(
			"t1".into(),
			Entity {
				id: "t1".into(),
				..Default::default()
			},
		);
		g.kerns.insert("generic".into(), child);
		if let Some(root) = g.kerns.get_mut(&root_id) {
			root.children.push("generic".into());
		}

		g.unload("generic").expect("unload spills to the store");
		assert!(g.is_unloaded("generic"), "precondition: child is unloaded");

		let g = Arc::new(RwLock::new(g));
		{
			let mut w = g.write();
			let evicted = evict_empty_children(&mut w, &root_id);
			assert!(!evicted, "an unloaded child is not an eviction candidate");
			let root_children = w.kerns.get(&root_id).unwrap().children.clone();
			assert!(
				root_children.contains(&"generic".to_string()),
				"unloaded child must stay registered under root"
			);
		}
		// And it comes back whole on access.
		let mut w = g.write();
		let back = w.get("generic").expect("auto-reload from the store");
		assert_eq!(back.entities.len(), 1, "entities survive the round-trip");
	}

	#[test]
	fn do_cluster_persists_each_spawned_child_before_the_parent() {
		let q = Queue::new(64);
		let mut g = GraphGnn::new();
		let mut kern = Kern::new("k", "");
		kern.graviton_text = "named".into();
		kern.graviton_vec = vec![1.0, 0.0];
		for i in 0..base::base_constants::KERN_MIN_CLUSTER_SIZE {
			let id = format!("e{i}");
			kern.entities.insert(
				id.clone(),
				Entity {
					id,
					vector: vec![0.0, 1.0].into(),
					..Default::default()
				},
			);
		}
		g.kerns.insert("k".into(), kern);
		let g = Arc::new(RwLock::new(g));

		do_cluster(&q, &g, "k", &TickConfig::default(), None, None);

		let children = g.read().kerns.get("k").unwrap().children.clone();
		assert!(!children.is_empty(), "precondition: a child spawned");

		let mut rx = q.take_receiver().unwrap();
		let mut persists = Vec::new();
		while let Ok(t) = rx.try_recv() {
			if matches!(t.kind, TaskKind::Persist) {
				persists.push(t.kern_id.clone());
			}
		}
		let parent_pos = persists
			.iter()
			.position(|k| k == "k")
			.expect("parent Persist enqueued");
		for c in &children {
			let child_pos = persists
				.iter()
				.position(|k| k == c)
				.unwrap_or_else(|| panic!("spawned child {c} gets its own Persist"));
			assert!(
				child_pos < parent_pos,
				"child Persist must run before the parent row drops the migrated entities"
			);
		}
	}

	#[test]
	fn spawning_a_cluster_carries_outgoing_reasons_and_reindexes_the_entity() {
		use base::base_types::{Kern, Reason};
		use graph::reason::add_reason;

		let mut g = GraphGnn::new();
		let root_id = g.root.id.clone();
		let mut parent = Kern::new("parent", &root_id);
		for id in ["moved", "stays"] {
			parent.entities.insert(
				id.into(),
				Entity {
					id: id.into(),
					..Default::default()
				},
			);
		}
		add_reason(
			&mut parent,
			Reason {
				id: "out".into(),
				from: "moved".into(),
				to: "stays".into(),
				..Default::default()
			},
		);
		g.register(parent);
		g.index_entity("moved", "parent");
		g.index_entity("stays", "parent");
		g.index_reason("out", "parent");

		let clusters = vec![Cluster {
			members: vec![Entity {
				id: "moved".into(),
				..Default::default()
			}],
		}];
		let children = spawn_child_clusters(&mut g, "parent", &clusters, &[0]);
		let child_id = children.first().expect("one child spawned").clone();

		assert_eq!(
			g.kern_of_entity("moved"),
			Some(child_id.as_str()),
			"entity->kern index must follow the entity into the child"
		);

		let child = g.kerns.get(&child_id).expect("child resident");
		assert!(child.entities.contains_key("moved"));
		assert!(
			child.reasons.contains_key("out"),
			"an edge lives in its `from` kern, so the outgoing reason moves too"
		);
		assert_eq!(
			g.kern_of_reason("out"),
			Some(child_id.as_str()),
			"reason->kern index must follow the reason"
		);

		let parent = g.kerns.get("parent").expect("parent resident");
		assert!(
			!parent.reasons.contains_key("out"),
			"the reason must not be left behind in the parent"
		);
		assert!(
			parent.entities.contains_key("stays"),
			"unclustered entities stay put"
		);
	}

	#[test]
	fn a_failed_cluster_migration_never_drops_the_entity() {
		let mut g = GraphGnn::new();
		let root_id = g.root.id.clone();
		let mut parent = base::base_types::Kern::new("parent", &root_id);
		parent.entities.insert(
			"e1".into(),
			Entity {
				id: "e1".into(),
				..Default::default()
			},
		);
		g.register(parent);

		// A member that is not actually in the source kern: the move must be rejected, not lossy.
		let clusters = vec![Cluster {
			members: vec![Entity {
				id: "ghost".into(),
				..Default::default()
			}],
		}];
		spawn_child_clusters(&mut g, "parent", &clusters, &[0]);

		assert!(
			g.kerns
				.get("parent")
				.expect("parent resident")
				.entities
				.contains_key("e1"),
			"a rejected move leaves the parent intact"
		);
	}

	#[test]
	fn a_panicking_task_is_contained_counted_and_still_accounted_for() {
		let q = Queue::new(8);
		let boom = task(TaskKind::GnnPropagate, "k");
		assert!(q.enqueue(boom.clone()));
		q.dequeued(&boom);

		run_guarded(&q, &boom, || panic!("gnn exploded"));

		let (count, last) = q.panics();
		assert_eq!(count, 1, "the panic is counted, not swallowed");
		let last = last.expect("the panic is retained for health reporting");
		assert_eq!(last.kind, TaskKind::GnnPropagate);
		assert_eq!(last.kern_id, "k");
		assert_eq!(last.message, "gnn exploded");
		assert_eq!(
			q.metrics().0,
			0,
			"a panicking task contributes no duration to the success mean"
		);
	}

	#[test]
	fn the_tick_loop_body_survives_a_panic_and_runs_the_next_task() {
		let q = Queue::new(8);
		let boom = task(TaskKind::Cluster, "dead");
		let next = task(TaskKind::Persist, "alive");

		run_guarded(&q, &boom, || panic!("boom"));
		let mut ran = false;
		run_guarded(&q, &next, || ran = true);

		assert!(ran, "the task after a panicking one still runs");
		assert_eq!(q.panics().0, 1, "only the panicking task is counted");
	}

	// `run_guarded` in isolation proves nothing about the loop: only this test fails
	// if `start` goes back to calling `process_task` unguarded.
	#[tokio::test]
	async fn start_contains_a_panicking_task_and_keeps_draining_the_queue() {
		let mut graph = GraphGnn::new();
		let mut k = base::base_types::Kern::new("k", "");
		let mut e = Entity {
			id: "e1".into(),
			..Default::default()
		};
		e.dirty = true;
		e.statements = vec!["needs a vector".into()];
		k.entities.insert("e1".into(), e);
		graph.kerns.insert("k".into(), k);
		graph.index_entity("e1", "k");
		let g = Arc::new(RwLock::new(graph));

		let q = Arc::new(Queue::new(8));
		assert!(q.enqueue(task(TaskKind::Reembed, "k")));
		assert!(q.enqueue(crate::tick_queue::task_commit_access(&["e1".to_string()])));

		let ctx = TickContext {
			llm: None,
			embed: Some(Arc::new(|_: &str| panic!("embed exploded"))),
			broadcast_q: None,
			gnn_cfg: GnnConfig::defaults(),
			tick_cfg: TickConfig::default(),
			heat_cfg: HeatConfig::default(),
		};
		start(q.clone(), g.clone(), ctx);

		for _ in 0..200 {
			if q.panics().0 == 1 && g.read().kerns["k"].entities["e1"].accessed_at.is_some() {
				break;
			}
			tokio::time::sleep(Duration::from_millis(5)).await;
		}

		let (count, last) = q.panics();
		assert_eq!(count, 1, "the panicking task is counted by the live loop");
		assert_eq!(last.expect("retained").kind, TaskKind::Reembed);
		assert_eq!(
			g.read().kerns["k"].entities["e1"].access_count.value(),
			1,
			"the task queued behind the panicking one still ran"
		);
	}

	// Moving a task off the loop must not quietly mean it never runs. The
	// assertion is on the EFFECT — `gnn_vector` populated — so a hand-off that
	// merely returns fails here no matter how long the test waits.
	#[tokio::test]
	async fn a_gnn_propagate_handed_off_the_loop_still_lands_its_embeddings() {
		use base::base_types::{mk_entity, EntityKind};

		let mut graph = GraphGnn::new();
		let mut k = Kern::new("k", "");
		for i in 0..4 {
			let id = format!("e{i}");
			k.entities
				.insert(id.clone(), mk_entity(&id, &id, 0.0, EntityKind::Claim));
		}
		for i in 0..3 {
			let (from, to) = (format!("e{i}"), format!("e{}", i + 1));
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
		graph.kerns.insert("k".into(), k);
		let g = Arc::new(RwLock::new(graph));

		let q = Arc::new(Queue::new(8));
		let ctx = TickContext {
			llm: None,
			embed: None,
			broadcast_q: None,
			gnn_cfg: GnnConfig {
				min_thoughts: 2,
				train_epochs: 2,
				..GnnConfig::defaults()
			},
			tick_cfg: TickConfig::default(),
			heat_cfg: HeatConfig::default(),
		};
		start(q.clone(), g.clone(), ctx);
		assert!(q.enqueue(task(TaskKind::GnnPropagate, "k")));

		let mut landed = false;
		for _ in 0..600 {
			landed = g.read().kerns["k"]
				.entities
				.values()
				.all(|e| !e.gnn_vector.is_empty());
			if landed {
				break;
			}
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		assert!(
			landed,
			"the propagation still has to happen once it stops running on the loop"
		);
		assert!(
			!g.read().kerns["k"].gnn_weights.is_empty(),
			"and its trained weights still reach the kern"
		);
	}

	// The arm itself, with no timing margin to hide behind: the runner never
	// returns, so `process_task` can only return if it handed the propagation to
	// the trainer instead of running it. Put the training back on the loop and the
	// handshake below is never sent.
	#[test]
	fn the_gnn_arm_hands_the_propagation_off_instead_of_running_it_on_the_loop() {
		let q = Queue::new(8);
		let g = Arc::new(RwLock::new(GraphGnn::new()));
		let (sink, reached) = std::sync::mpsc::sync_channel::<()>(1);
		let trainer = crate::tick_trainer::Trainer::spawn(Arc::new(Queue::new(8)), move |_| {
			let _ = sink.send(());
			std::thread::sleep(Duration::from_secs(3600));
		});
		let ctx = TickContext {
			llm: None,
			embed: None,
			broadcast_q: None,
			gnn_cfg: GnnConfig::defaults(),
			tick_cfg: TickConfig::default(),
			heat_cfg: HeatConfig::default(),
		};

		process_task(
			&q,
			&g,
			&task(TaskKind::GnnPropagate, "k"),
			&ctx,
			Some(&trainer),
		);

		reached
			.recv_timeout(Duration::from_secs(5))
			.expect("the propagation must reach the trainer, not the loop");
	}

	#[test]
	fn panic_message_reads_str_string_and_unknown_payloads() {
		assert_eq!(panic_message(&"literal"), "literal");
		assert_eq!(panic_message(&"formatted".to_string()), "formatted");
		assert_eq!(panic_message(&7u8), "unknown panic payload");
	}

	#[test]
	fn do_cluster_skips_gnn_when_no_structural_work() {
		let q = Queue::new(64);
		let mut g = GraphGnn::new();
		let root_id = g.root.id.clone();
		if let Some(k) = g.kerns.get_mut(&root_id) {
			k.entities.insert(
				"e1".into(),
				Entity {
					id: "e1".into(),
					..Default::default()
				},
			);
		}
		let g = Arc::new(RwLock::new(g));

		do_cluster(&q, &g, &root_id, &TickConfig::default(), None, None);

		let mut rx = q.take_receiver().unwrap();
		let mut kinds = Vec::new();
		while let Ok(t) = rx.try_recv() {
			kinds.push(t.kind);
		}
		let gnn = kinds
			.iter()
			.filter(|k| matches!(k, TaskKind::GnnPropagate))
			.count();
		assert_eq!(gnn, 0, "no structural change -> GNN propagation skipped");
	}
}
