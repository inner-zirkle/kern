use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The directory kern was invoked from, captured before `main` re-pins cwd to
/// the project root.
///
/// The re-pin is right for the store (a subdir launch must not boot an empty
/// graph) but wrong for every path a caller typed: those mean what they meant in
/// the caller's cwd. Anything reading a user-supplied relative path must go
/// through [`launch_dir_join`], not `std::fs` directly.
static LAUNCH_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Record the launch dir. Called once from `main` before the re-pin; later calls
/// are ignored, so a test or an embedder cannot corrupt it mid-run.
pub fn set_launch_dir(dir: PathBuf) {
	let _ = LAUNCH_DIR.set(dir);
}

/// Resolve a caller-supplied path against the launch dir. Absolute paths pass
/// through untouched; a relative one is joined to where the caller actually
/// stood. Falls back to the path as given when no launch dir was recorded (a
/// library embedder that never re-pinned), which is the pre-existing behaviour.
pub fn launch_dir_join(path: impl AsRef<Path>) -> PathBuf {
	let p = path.as_ref();
	if p.is_absolute() {
		return p.to_path_buf();
	}
	match LAUNCH_DIR.get() {
		Some(dir) => dir.join(p),
		None => p.to_path_buf(),
	}
}

pub mod accept;
pub mod base_constants;
pub mod commands;
pub(crate) mod commands_admin;
pub(crate) mod commands_graph_ops;
mod commands_ingest_cmd;
mod commands_intake_cmd;
mod commands_mcp_cmd;
mod commands_mcp_restart;
mod commands_profile_cmd;
mod commands_query;
mod commands_reembed;
mod commands_route;
mod commands_status;
pub mod diskann;
pub mod graph;
pub mod health;
pub mod heat;
pub mod hnsw;
pub mod identity;
pub mod lexical;
pub mod lock;
pub mod log_throttle;
pub mod math;
pub mod merge;
pub mod persist;
pub mod reason;
pub mod search;
pub mod base_store;
pub mod time;
pub mod base_types;
pub mod util;
pub mod validate;
pub mod vector_backend;
pub mod config;
pub mod config_detached_log;
mod config_embed;
mod config_gnn;
mod config_gossip;
mod config_graph;
mod config_hub;
mod config_ingest;
mod config_intake;
pub mod config_io;
mod config_preset;
mod config_reason;
mod config_reload;
mod config_retrieval;
mod config_secrets;
mod config_serve;
mod config_tick;
mod config_watcher;
pub mod crdt;
pub mod gnn;
pub mod gnn_activation;
pub mod gnn_backward;
pub mod gnn_gcn;
pub mod gnn_graph;
pub mod gnn_layer;
pub mod gnn_loss;
pub mod gnn_model;
pub mod gnn_norm;
pub mod gnn_optim;
pub mod gnn_persist;
pub mod gnn_propagate;
pub mod gnn_sparse;
pub mod gnn_tensor;
pub mod gossip_contract;
pub mod gossip_discovery;
pub mod gossip_handler;
pub mod gossip_identity;
pub mod gossip_ledger;
pub mod gossip_node;
pub mod gossip_privacy;
pub mod gossip_rate;
pub mod gossip_ring;
pub mod gossip_seen;
pub mod gossip_subs;
pub mod gossip_transport;
pub mod gossip_types;
pub mod hub;
pub mod hub_node;
pub mod hub_serve;
pub mod ingest;
pub mod ingest_config;
pub mod ingest_dedup;
pub mod ingest_direct;
pub mod ingest_distill;
pub mod ingest_embed;
pub mod ingest_file_watcher;
pub mod ingest_intake;
pub mod ingest_intake_status;
pub mod ingest_outcome;
pub mod ingest_place;
pub mod ingest_split;
pub mod ingest_worker;
pub mod llm;
pub mod mcp;
pub mod profile;
pub mod quant;
pub mod retrieval;
pub mod rpc;
pub mod rpc_kern_rpc_server;
pub mod store;
pub mod takeover;
pub mod tick;
pub mod tick_cluster;
pub mod tick_gnn_propagate;
pub mod tick_idle;
pub mod tick_pulse;
pub mod tick_queue;
pub mod tick_stigmergy;
pub mod tick_tasks;
pub mod tick_trainer;
pub mod types;
pub mod transport;
pub mod watcher;
pub mod watcher_event;
pub mod watcher_file;
pub mod watcher_ignore_rules;
pub mod watcher_pipeline;

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod launch_dir_tests {
	use super::*;

	// `set_launch_dir` is a OnceLock and the whole test binary shares one process,
	// so these assert the two branches that hold regardless of who won the set:
	// absolute paths are never rewritten, and a relative path never silently
	// resolves against a *different* directory than the one recorded.

	#[test]
	fn absolute_paths_pass_through_untouched() {
		let abs = Path::new("/tmp/kern-launch-dir-probe.md");
		assert_eq!(launch_dir_join(abs), abs.to_path_buf());
	}

	#[test]
	fn relative_paths_resolve_against_the_recorded_launch_dir() {
		// Whatever the recorded dir is, a relative join must end with the given
		// path and must not be bare — that bareness was the bug: the caller's path
		// got read relative to the re-pinned project root instead.
		let joined = launch_dir_join("notes.md");
		assert!(joined.ends_with("notes.md"));
		match LAUNCH_DIR.get() {
			Some(dir) => assert_eq!(joined, dir.join("notes.md")),
			None => assert_eq!(joined, PathBuf::from("notes.md")),
		}
	}

	#[test]
	fn a_recorded_launch_dir_is_used_verbatim() {
		set_launch_dir(PathBuf::from("/tmp/kern-launch-probe"));
		// Either this call won the OnceLock or an earlier one did; either way the
		// resolution must agree with whatever is recorded, never with cwd.
		let dir = LAUNCH_DIR.get().expect("launch dir set by now");
		assert_eq!(launch_dir_join("x/y.md"), dir.join("x/y.md"));
	}
}
