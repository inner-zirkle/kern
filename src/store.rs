use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use std::time::Instant;

use crate::graph::GraphGnn;
use crate::config::Config;
use crate::ingest::Worker;
use crate::llm::Client as LlmClient;
use crate::tick_queue::Queue;
use crate::tick_tasks::{BroadcastQuestionFunc, EmbedFunc, LlmFunc as TickLlmFunc};

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
		broadcast_q: Option<BroadcastQuestionFunc>,
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

		let graph = Arc::new(RwLock::new(crate::commands::load_graph(&store_cfg)));

		// The one persist closure; guarded flush won't overwrite a graph another writer grew on disk.
		let save_g = graph.clone();
		let save_cfg = store_cfg.clone();
		let save_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
			crate::commands::save_graph_guarded(&save_g, &save_cfg);
		});

		let tick_q = Arc::new(Queue::new(cfg.tick.queue_capacity.max(1)));

		let defer_q = tick_q.clone();
		let defer: crate::ingest::worker::DeferQuestionsFn = Arc::new(move |entity_id: &str| {
			let _ = defer_q.enqueue(crate::tick_queue::task_extra(
				crate::tick_queue::TaskKind::SeedQuestions,
				"",
				entity_id,
			));
		});

		let contra_q = tick_q.clone();
		let defer_contradiction: crate::ingest::worker::DeferContradictionFn =
			Arc::new(move |kern_id: &str, reason_id: &str| {
				let _ = contra_q.enqueue(crate::tick_queue::task_extra(
					crate::tick_queue::TaskKind::ClassifyContradiction,
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

		let tick_handle = crate::tick::start(
			tick_q.clone(),
			graph.clone(),
			crate::tick::TickContext {
				llm: tick_llm,
				embed: tick_embed,
				broadcast_q,
				gnn_cfg: cfg.gnn.into(),
				tick_cfg: cfg.tick,
				heat_cfg: cfg.heat,
			},
		);

		crate::tick::enqueue_all(&tick_q, &graph);

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
mod tests {
	use super::*;
	use std::time::Duration;

	fn dead_client() -> LlmClient {
		LlmClient::new_embed_only("http://127.0.0.1:1", "test", "")
	}

	#[tokio::test]
	async fn open_dedups_and_touches_on_reopen() {
		let dir = tempfile::tempdir().unwrap();
		let reg = Registry::new();
		let cfg = Config::default();

		let a = reg.open(dir.path(), &cfg, dead_client(), None, None, None);
		let first_touch = *a.last_touch.read();

		tokio::time::sleep(Duration::from_millis(2)).await;

		let b = reg.open(dir.path(), &cfg, dead_client(), None, None, None);
		assert!(
			Arc::ptr_eq(&a, &b),
			"re-open of the same dir returns the same StoreEntry"
		);
		assert_eq!(reg.len(), 1, "no duplicate store registered");
		assert!(
			*b.last_touch.read() > first_touch,
			"last_touch advanced on re-open",
		);
	}

	#[tokio::test]
	async fn distinct_dirs_get_isolated_stores() {
		let dir_a = tempfile::tempdir().unwrap();
		let dir_b = tempfile::tempdir().unwrap();
		let reg = Registry::new();
		let cfg = Config::default();

		let a = reg.open(dir_a.path(), &cfg, dead_client(), None, None, None);
		let b = reg.open(dir_b.path(), &cfg, dead_client(), None, None, None);

		assert_eq!(reg.len(), 2, "one store per distinct dir");
		assert!(
			!Arc::ptr_eq(&a.graph, &b.graph),
			"distinct dirs never share a graph"
		);
		assert_ne!(a.key, b.key, "distinct dirs get distinct keys");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn concurrent_open_of_same_dir_yields_one_store() {
		let dir = tempfile::tempdir().unwrap();
		let reg = Arc::new(Registry::new());
		let cfg = Config::default();
		let path = dir.path().to_path_buf();

		let mut handles = Vec::new();
		for _ in 0..4 {
			let reg = reg.clone();
			let cfg = cfg.clone();
			let path = path.clone();
			handles.push(tokio::spawn(async move {
				reg.open(&path, &cfg, dead_client(), None, None, None)
			}));
		}
		let mut entries = Vec::new();
		for h in handles {
			entries.push(h.await.unwrap());
		}

		for e in &entries[1..] {
			assert!(
				Arc::ptr_eq(&entries[0], e),
				"all concurrent opens share one StoreEntry"
			);
		}
		assert_eq!(
			reg.len(),
			1,
			"exactly one store registered despite the race"
		);
	}
}
