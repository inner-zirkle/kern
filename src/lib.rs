//! kern — a self-organising knowledge-graph memory daemon.
//!
//! Text enters through the ingest pipeline ([`ingest`]), is distilled and
//! embedded into a bitemporal entity graph ([`graph`], persisted via LMDB in
//! [`store_core`]), and is retrieved by hybrid vector + lexical + graph-walk
//! search ([`retrieval`]) whose weights a small GNN refines over time
//! ([`gnn`], [`tick`]). The graph is local and single-writer; a long-running
//! per-root daemon owns it and serves a typed RPC ([`transport`], [`rpc`]),
//! with the CLI in [`commands`] as a thin dispatch to that daemon. One machine
//! hub ([`hub`]) reaches every per-root daemon, so a store is shared across
//! repos without any of them leaving the machine.
//!
//! Alpha: persisted formats are versioned, never migrated — see `FORMAT_VERSION`
//! in [`store_core`] and `WEIGHT_FILE_VERSION` in [`gnn`].

pub use ::commands;
pub use ::hub;
pub use ::rpc;
pub use ::store as store_registry;

#[cfg(test)]
mod global_allocator {
	use test_support::alloc_probe::Counting;
	#[global_allocator]
	static COUNTING: Counting = Counting;
}
