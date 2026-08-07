//! Graph bootstrap: load, bind, save (guarded/unguarded), reconcile.
//!
//! These are the operations the daemon registry and the CLI both reach for
//! when wiring a graph to a store. They live in their own crate (no `kern`
//! dependency, no cycle with `store::Registry`).

use std::sync::Arc;

use graph::graph::GraphGnn;
use parking_lot::RwLock;
use store_core;

pub type SharedGraph = Arc<RwLock<GraphGnn>>;

pub fn apply_graph_config(g: &mut GraphGnn, cfg: &config::GraphConfig) {
	g.set_max_loaded_kerns(cfg.max_kerns);
	g.set_disk_threshold(cfg.disk_threshold);
	if cfg.disk_threshold != base::base_constants::KERN_CAP_DISABLED {
		g.rebuild_index();
	}
}

pub fn load_graph(cfg: &config::Config) -> GraphGnn {
	let mut g = match graph::persist::load_dir(&cfg.data_dir) {
		Ok(g) => g,
		Err(e) => {
			// The empty fallback boots at epoch 0, so its flushes are refused
			// against a non-empty store and absorb disk instead — but a silent
			// fallback here is how a wiped store went undiagnosed. Say it.
			tracing::error!(
				target: "kern.persist",
				error = %e,
				data_dir = %cfg.data_dir,
				"graph load failed — starting empty at epoch 0 (flushes will refuse and absorb)"
			);
			let mut g = GraphGnn::new();
			g.data_dir = cfg.data_dir.clone();
			if let Ok(store) = store_core::Store::open(&cfg.data_dir) {
				g.set_store(std::sync::Arc::new(store));
			}
			g
		}
	};
	bind_embed_model(&mut g, cfg);
	apply_graph_config(&mut g, &cfg.graph);
	if let Some(lex) = g.lexical() {
		lex.set_bm25_params(cfg.retrieval.bm25_k1 as f32, cfg.retrieval.bm25_b as f32);
	}
	g
}

// Every store handle in this process is bound to the configured embedding model
// here — the stamp is what turns a silent model swap into a reported one.
pub fn bind_embed_model(g: &mut GraphGnn, cfg: &config::Config) {
	g.set_embed_model(&cfg.embed.model);
	graph::persist::check_graph_stamp(g);
}

// Writes the whole kern map with no epoch check, so a commit that landed since
// this graph was loaded is overwritten unseen. Only safe while the caller holds
// the writer lock (`gc`, `compact`, `reembed`) or owns the dir outright. Anything
// else wants `save_graph_guarded`, which refuses a stale flush and absorbs.
pub fn save_graph_unguarded(g: &GraphGnn) {
	if let Err(e) = graph::persist::save_all(g) {
		eprintln!("save: {e}");
	}
}

pub fn reload_graph(cfg: &config::Config, old: &GraphGnn) -> GraphGnn {
	match graph::persist::reload_from_disk(old) {
		Some(mut g) => {
			bind_embed_model(&mut g, cfg);
			apply_graph_config(&mut g, &cfg.graph);
			if let Some(lex) = g.lexical() {
				lex.set_bm25_params(cfg.retrieval.bm25_k1 as f32, cfg.retrieval.bm25_b as f32);
			}
			g
		}
		None => load_graph(cfg),
	}
}

pub fn save_graph_guarded(
	graph: &std::sync::Arc<parking_lot::RwLock<GraphGnn>>,
	cfg: &config::Config,
) {
	const FLUSH_RETRIES: u32 = 5;
	for attempt in 0..FLUSH_RETRIES {
		let (snapshot, expected) = {
			let g = graph.read();
			(graph::persist::snapshot_for_flush(&g), g.flushed_epoch())
		};
		let Some(snapshot) = snapshot else {
			return;
		};
		let outcome = graph::persist::flush_snapshot(&snapshot, expected);
		match outcome {
			Ok(store_core::FlushOutcome::Flushed { epoch }) => {
				graph.write().set_flushed_epoch(epoch);
				return;
			}
			Ok(store_core::FlushOutcome::RefusedStale {
				disk_epoch,
				expected,
			}) => {
				tracing::warn!(
					target: "kern.persist",
					disk_epoch,
					expected,
					attempt,
					data_dir = %cfg.data_dir,
					"refused to flush a stale snapshot — disk advanced under us (another writer); absorbing disk rows and retrying"
				);
				let mut w = graph.write();
				let Some(fresh) = graph::persist::reload_from_disk(&w) else {
					tracing::error!(
						target: "kern.persist",
						data_dir = %cfg.data_dir,
						"reload after a refused flush failed (unreadable or rootless store); unflushed rows stay in memory until the next snapshot"
					);
					return;
				};
				let disk_epoch = fresh.flushed_epoch();
				graph::merge::absorb_graph(&mut w, fresh);
				w.set_flushed_epoch(disk_epoch);
			}
			Err(e) => {
				tracing::error!(
					target: "kern.persist",
					error = %e,
					data_dir = %cfg.data_dir,
					"flush failed; unflushed rows stay in memory until the next snapshot"
				);
				eprintln!("save: {e}");
				return;
			}
		}
	}
	tracing::warn!(
		target: "kern.persist",
		data_dir = %cfg.data_dir,
		"flush still refused after {FLUSH_RETRIES} absorb-and-retry rounds; unflushed rows stay in memory until the next snapshot"
	);
}

pub fn snapshot_if_dirty(
	graph: &SharedGraph,
	cfg: &config::Config,
	last_snap_epoch: &mut u64,
) -> bool {
	let epoch = graph.read().mutation_epoch();
	if epoch == *last_snap_epoch {
		return false;
	}
	save_graph_guarded(graph, cfg);
	*last_snap_epoch = epoch;
	true
}

pub fn reconcile_if_stale(
	graph: &std::sync::Arc<parking_lot::RwLock<GraphGnn>>,
	cfg: &config::Config,
) -> bool {
	let mut w = graph.write();
	let stale = match w.store() {
		Some(store) => store.read_epoch() > w.flushed_epoch(),
		None => false,
	};
	if stale {
		// Reload reusing the open store handle: load_graph would double-open the
		// LMDB env on a dir already open in this process.
		let fresh = reload_graph(cfg, &w);
		*w = fresh;
		tracing::info!(
			target: "kern.persist",
			data_dir = %cfg.data_dir,
			"store advanced under the daemon (external write); reloaded graph from disk"
		);
	}
	stale
}
