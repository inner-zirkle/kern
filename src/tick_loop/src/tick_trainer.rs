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
mod tests {
	use super::*;
	use std::sync::mpsc::channel;
	use std::time::Duration;

	// A runner that blocks until released, so "a second request arrives while the
	// first is in flight" is a state the test holds open rather than races for.
	struct Held {
		trainer: Trainer,
		ran: std::sync::mpsc::Receiver<String>,
		release: SyncSender<()>,
	}

	fn held_trainer(q: Arc<Queue>) -> Held {
		let (ran_tx, ran) = channel::<String>();
		let (release, gate) = sync_channel::<()>(0);
		let trainer = Trainer::spawn(q, move |kern_id| {
			let _ = ran_tx.send(kern_id.to_string());
			let _ = gate.recv();
		});
		Held {
			trainer,
			ran,
			release,
		}
	}

	#[test]
	fn a_repeat_request_for_a_waiting_kern_is_coalesced_not_queued_twice() {
		let h = held_trainer(Arc::new(Queue::new(8)));

		assert_eq!(h.trainer.submit("busy"), Submit::Accepted);
		// It is now running and blocked; "busy" left the waiting set.
		assert_eq!(
			h.ran.recv_timeout(Duration::from_secs(5)).unwrap(),
			"busy",
			"the first request runs"
		);

		assert_eq!(
			h.trainer.submit("k"),
			Submit::Accepted,
			"a kern nobody is waiting on is admitted behind the running one"
		);
		assert_eq!(
			h.trainer.submit("k"),
			Submit::Coalesced,
			"a second request for the SAME waiting kern is folded into it"
		);
		assert_eq!(
			h.trainer.submit("k"),
			Submit::Coalesced,
			"and so is a third"
		);

		h.release.send(()).unwrap();
		assert_eq!(
			h.ran.recv_timeout(Duration::from_secs(5)).unwrap(),
			"k",
			"the coalesced kern still trains exactly once"
		);
		h.release.send(()).unwrap();
		assert!(
			h.ran.recv_timeout(Duration::from_millis(300)).is_err(),
			"the folded requests do not each become their own training run"
		);
	}

	#[test]
	fn a_backlog_past_the_cap_is_refused_and_counted_not_grown() {
		// Declared first so it outlives `h`: the trainer must be dropped, and its
		// thread with it, while this still holds.
		let _serial = REFUSAL_COUNTER.blocking_lock();
		let h = held_trainer(Arc::new(Queue::new(8)));
		let before = gnn_train_refused();

		assert_eq!(h.trainer.submit("running"), Submit::Accepted);
		h.ran.recv_timeout(Duration::from_secs(5)).unwrap();

		for i in 0..TRAIN_QUEUE_CAP {
			assert_eq!(
				h.trainer.submit(&format!("w{i}")),
				Submit::Accepted,
				"kern w{i} fits inside the cap"
			);
		}
		assert_eq!(
			h.trainer.submit("one-too-many"),
			Submit::Refused,
			"past the cap the NEWEST request is refused, never queued"
		);
		assert_eq!(
			gnn_train_refused() - before,
			1,
			"the refusal is counted, not just dropped"
		);
		assert_eq!(
			h.trainer.submit("one-too-many"),
			Submit::Refused,
			"a refused kern is not left marked as waiting forever"
		);

		h.release.send(()).unwrap();
	}

	#[test]
	fn a_panicking_propagation_is_counted_and_the_trainer_keeps_training() {
		let q = Arc::new(Queue::new(8));
		let (ran_tx, ran) = channel::<String>();
		let trainer = Trainer::spawn(q.clone(), move |kern_id| {
			let _ = ran_tx.send(kern_id.to_string());
			if kern_id == "boom" {
				panic!("gnn exploded");
			}
		});

		assert_eq!(trainer.submit("boom"), Submit::Accepted);
		assert_eq!(ran.recv_timeout(Duration::from_secs(5)).unwrap(), "boom");

		for _ in 0..500 {
			if q.panics().0 == 1 {
				break;
			}
			std::thread::sleep(Duration::from_millis(5));
		}
		let (count, last) = q.panics();
		assert_eq!(count, 1, "the panic reaches the same counter health reads");
		let last = last.expect("retained for health reporting");
		assert_eq!(last.kind, TaskKind::GnnPropagate);
		assert_eq!(last.kern_id, "boom");
		assert_eq!(last.message, "gnn exploded");

		assert_eq!(trainer.submit("after"), Submit::Accepted);
		assert_eq!(
			ran.recv_timeout(Duration::from_secs(5)).unwrap(),
			"after",
			"the trainer thread survived the panic and ran the next kern"
		);
	}

	#[test]
	fn dropping_the_trainer_stops_its_thread() {
		let (ran_tx, ran) = channel::<String>();
		let trainer = Trainer::spawn(Arc::new(Queue::new(8)), move |kern_id| {
			let _ = ran_tx.send(kern_id.to_string());
		});
		assert_eq!(trainer.submit("k"), Submit::Accepted);
		assert_eq!(ran.recv_timeout(Duration::from_secs(5)).unwrap(), "k");
		drop(trainer);
		// Disconnected, NOT merely Timeout: the runner owns `ran_tx`, so the channel
		// only breaks once the thread has actually ended. A timeout would be the
		// same `is_err()` and would prove nothing.
		assert_eq!(
			ran.recv_timeout(Duration::from_secs(5)),
			Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
			"the sender is gone, so the thread ends instead of outliving the store"
		);
	}
}
