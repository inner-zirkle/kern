//! The per-process registry of open stores. One daemon serves many data dirs;
//! each [`StoreEntry`] bundles a dir's graph, ingest worker, tick queue, and
//! the single persist closure, keyed by canonical path so two callers naming
//! the same dir share one instance (LMDB forbids a double-open).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use std::time::Instant;

use config::Config;
use graph::graph::GraphGnn;
use ingest::Worker;
use llm::Client as LlmClient;
use tick::tick_queue::Queue;
use tick_loop::tick_tasks::{EmbedFunc, LlmFunc as TickLlmFunc};

pub type StoreKey = PathBuf;

pub struct StoreEntry {
	pub key: StoreKey,
	pub graph: Arc<RwLock<GraphGnn>>,
	pub worker: Arc<Worker>,
	pub tick_q: Arc<Queue>,
	pub tick_handle: tokio::task::JoinHandle<()>,
	// Single instance per store — clone this, never build a duplicate persist closure.
	pub save_fn: Arc<dyn Fn() + Send + Sync>,
	pub last_touch: RwLock<Instant>,
}

#[derive(Default)]
pub struct Registry {
	stores: RwLock<HashMap<StoreKey, Arc<StoreEntry>>>,
	// Per-key build locks serialize concurrent `open()`s of the SAME dir so a losing
	// racer can't orphan its already-spawned worker/tick onto a dropped graph.
	builds: Mutex<HashMap<StoreKey, Arc<Mutex<()>>>>,
}

impl Registry {
	pub fn new() -> Self {
		Self::default()
	}

	fn canon(p: &Path) -> StoreKey {
		std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
	}

	pub fn get(&self, data_dir: &Path) -> Option<Arc<StoreEntry>> {
		self.stores.read().get(&Self::canon(data_dir)).cloned()
	}

	pub fn len(&self) -> usize {
		self.stores.read().len()
	}

	pub fn is_empty(&self) -> bool {
		self.stores.read().is_empty()
	}

	pub fn open(
		&self,
		data_dir: &Path,
		cfg: &Config,
		llm_client: LlmClient,
		tick_llm: Option<TickLlmFunc>,
		tick_embed: Option<EmbedFunc>,
	) -> Arc<StoreEntry> {
		let key = Self::canon(data_dir);
		if let Some(e) = self.stores.read().get(&key) {
			*e.last_touch.write() = Instant::now();
			return e.clone();
		}

		let build_lock = self.builds.lock().entry(key.clone()).or_default().clone();
		let _build = build_lock.lock();

		// Re-check under the build lock: a prior builder may have inserted while we waited.
		if let Some(e) = self.stores.read().get(&key) {
			*e.last_touch.write() = Instant::now();
			return e.clone();
		}

		let mut store_cfg = cfg.clone();
		store_cfg.data_dir = data_dir.to_string_lossy().into_owned();

		let graph = Arc::new(RwLock::new(bootstrap::load_graph(&store_cfg)));

		// The one persist closure; guarded flush won't overwrite a graph another writer grew on disk.
		let save_g = graph.clone();
		let save_cfg = store_cfg.clone();
		let save_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
			bootstrap::save_graph_guarded(&save_g, &save_cfg);
		});

		let tick_q = Arc::new(Queue::new(cfg.tick.queue_capacity.max(1)));

		let defer_q = tick_q.clone();
		let defer: ingest::worker::DeferQuestionsFn = Arc::new(move |entity_id: &str| {
			let _ = defer_q.enqueue(tick::tick_queue::task_extra(
				tick::tick_queue::TaskKind::SeedQuestions,
				"",
				entity_id,
			));
		});

		let contra_q = tick_q.clone();
		let defer_contradiction: ingest::worker::DeferContradictionFn =
			Arc::new(move |kern_id: &str, reason_id: &str| {
				let _ = contra_q.enqueue(tick::tick_queue::task_extra(
					tick::tick_queue::TaskKind::ClassifyContradiction,
					kern_id,
					reason_id,
				));
			});

		let worker = Arc::new(Worker::new(
			graph.clone(),
			llm_client,
			Some(defer),
			Some(defer_contradiction),
			Some(save_fn.clone()),
		));

		let tick_handle = tick_loop::start(
			tick_q.clone(),
			graph.clone(),
			tick_loop::TickContext {
				llm: tick_llm,
				embed: tick_embed,
				gnn_cfg: cfg.gnn.into(),
				tick_cfg: cfg.tick,
				heat_cfg: cfg.heat,
			},
		);

		tick_loop::enqueue_all(&tick_q, &graph);

		let entry = Arc::new(StoreEntry {
			key: key.clone(),
			graph,
			worker,
			tick_q,
			tick_handle,
			save_fn,
			last_touch: RwLock::new(Instant::now()),
		});

		self
			.stores
			.write()
			.entry(key)
			.or_insert_with(|| entry.clone())
			.clone()
	}
}

#[cfg(test)]
#[path = "tests/registry_test.rs"]
mod registry_tests;
