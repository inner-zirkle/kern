//! The single GNN training thread. Kerns submit snapshots through a small
//! bounded channel; one trains at a time, repeats for a kern already waiting
//! are coalesced, and overflow is refused and counted rather than queued —
//! training is advisory, the tick loop must never block on it.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;

use parking_lot::Mutex;

use util::LogThrottle;

use tick::tick_queue::{task, Queue, TaskKind};

// Distinct kerns that may wait behind the one training. Small on purpose: a
// waiting job holds a kern id, but the graph state it will train on is whatever
// the graph looks like when it finally runs, so a deep backlog buys staleness,
// not throughput.
const TRAIN_QUEUE_CAP: usize = 8;
const REFUSED_WARN_SECS: u64 = 60;
static TRAIN_REFUSED: AtomicU64 = AtomicU64::new(0);
static REFUSED_WARN: LogThrottle = LogThrottle::new(REFUSED_WARN_SECS);

// Propagations refused because the trainer was already `TRAIN_QUEUE_CAP` kerns
// behind. Those kerns keep their previous `gnn_vector` until something enqueues
// them again, and only the count says how often that happened.
pub fn gnn_train_refused() -> u64 {
	TRAIN_REFUSED.load(Ordering::Relaxed)
}

// `TRAIN_REFUSED` is process-global and `cargo test` — what CI runs, rather than
// the `cargo nextest` of `just test` — puts the whole suite in one process on
// many threads. Every test that *moves* the counter must therefore hold this
// while any test is *measuring* it, because a measurement is two reads and the
// gap between them is where somebody else's refusals land. Measured before it
// was added: the cap test below failed 5 runs in 30 once a second refusing test
// existed. Test-only, since nothing in production reads the counter twice and
// expects the two reads to agree.
//
// `tokio::sync::Mutex` rather than the `parking_lot` one this file already uses,
// because one of the holders measures across an `.await` on the RPC handler and
// a sync guard held over that is `clippy::await_holding_lock`. Sync callers take
// `blocking_lock()`, which is sound here only because every one of them is a
// plain `#[test]` with no runtime under it.
pub static REFUSAL_COUNTER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submit {
	Accepted,
	// Already waiting. A second request for the same kern is not a second
	// answer: the waiting job snapshots the graph when it RUNS, so it already
	// covers everything the newer request would have seen.
	Coalesced,
	Refused,
}

// One propagation at a time, on a thread of its own. Not `spawn_blocking`: that
// pool is 512 wide, so every kern would train at once and each training
// allocates a dense num_entities^2 adjacency.
pub struct Trainer {
	tx: SyncSender<String>,
	waiting: Arc<Mutex<HashSet<String>>>,
}

impl Trainer {
	pub fn spawn(q: Arc<Queue>, run: impl Fn(&str) + Send + 'static) -> Self {
		let (tx, rx) = sync_channel::<String>(TRAIN_QUEUE_CAP);
		let waiting = Arc::new(Mutex::new(HashSet::new()));
		let w = waiting.clone();
		std::thread::Builder::new()
			.name("kern-gnn".into())
			.spawn(move || {
				while let Ok(kern_id) = rx.recv() {
					// Cleared BEFORE the run, not after: a request arriving while this one
					// trains describes graph state this job's snapshot will not contain.
					w.lock().remove(&kern_id);
					// Without this the thread dies on the first panicking propagation and
					// every later one is silently never trained — a worse blast radius
					// than the tick loop's, which `run_guarded` already contains.
					let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&kern_id)));
					if let Err(payload) = r {
						let message = crate::tick::panic_message(payload.as_ref());
						tracing::error!(
							target: "kern.gnn",
							kern = %kern_id,
							panic = %message,
							"gnn training panicked; the trainer continues and this kern keeps its previous embeddings"
						);
						q.record_task_panic(&task(TaskKind::GnnPropagate, &kern_id), &message);
					}
				}
			})
			.expect("spawn gnn trainer thread");
		Self { tx, waiting }
	}

	pub fn submit(&self, kern_id: &str) -> Submit {
		{
			let mut waiting = self.waiting.lock();
			if !waiting.insert(kern_id.to_string()) {
				return Submit::Coalesced;
			}
		}
		if self.tx.try_send(kern_id.to_string()).is_err() {
			self.waiting.lock().remove(kern_id);
			let total = TRAIN_REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
			if REFUSED_WARN.allow() {
				tracing::warn!(
					target: "kern.gnn",
					cap = TRAIN_QUEUE_CAP,
					kern = %kern_id,
					total_refused = total,
					"gnn trainer is full; refusing the propagation (further refusals counted, not logged)"
				);
			}
			return Submit::Refused;
		}
		Submit::Accepted
	}
}

#[cfg(test)]
#[path = "tests/tick_trainer_test.rs"]
mod tick_trainer_tests;
