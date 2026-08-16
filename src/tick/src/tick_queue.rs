//! The bounded, deduplicating tick task queue: one slot per `(kind, kern)` so
//! a slow consumer coalesces repeat requests instead of queueing them, with
//! fault counters per task kind for the health surface.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskKind {
	Cluster,
	Name,
	Enrich,
	ResolveQuestion,
	// extra = entity id
	SeedQuestions,
	// extra = reason id
	ClassifyContradiction,
	Persist,
	GnnPropagate,
	StigmergyGc,
	Reembed,
	DiskConsolidate,
	// graph-global; kern_id empty
	IdleSweep,
	// extra = newline-joined entity ids; kern_id empty
	CommitAccess,
}

#[derive(Debug, Clone)]
pub struct Task {
	pub kind: TaskKind,
	pub kern_id: String,
	pub extra: String,
}

// A task that died (panic) or gave up (contained error). Both are degraded
// maintenance an operator must be able to see without scraping logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskFault {
	pub kind: TaskKind,
	pub kern_id: String,
	pub message: String,
}

impl std::fmt::Display for TaskFault {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.kern_id.is_empty() {
			write!(f, "{:?}: {}", self.kind, self.message)
		} else {
			write!(f, "{:?}[{}]: {}", self.kind, self.kern_id, self.message)
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TaskKey {
	kind: TaskKind,
	kern_id: String,
	extra: String,
}

fn key_of(t: &Task) -> TaskKey {
	TaskKey {
		kind: t.kind,
		kern_id: t.kern_id.clone(),
		extra: t.extra.clone(),
	}
}

pub struct Queue {
	tx: mpsc::Sender<Task>,
	rx: Mutex<Option<mpsc::Receiver<Task>>>,
	pending: Mutex<HashMap<TaskKey, bool>>,
	inflight: std::sync::atomic::AtomicUsize,
	stats: Mutex<(i64, Duration)>,
	panics: Mutex<(u64, Option<TaskFault>)>,
	failures: Mutex<(u64, Option<TaskFault>)>,
}

impl Queue {
	pub fn new(size: usize) -> Self {
		let (tx, rx) = mpsc::channel(size);
		Self {
			tx,
			rx: Mutex::new(Some(rx)),
			pending: Mutex::new(HashMap::new()),
			inflight: std::sync::atomic::AtomicUsize::new(0),
			stats: Mutex::new((0, Duration::ZERO)),
			panics: Mutex::new((0, None)),
			failures: Mutex::new((0, None)),
		}
	}

	pub fn take_receiver(&self) -> Option<mpsc::Receiver<Task>> {
		self.rx.lock().take()
	}

	pub fn enqueue(&self, t: Task) -> bool {
		let k = key_of(&t);
		{
			let mut pending = self.pending.lock();
			if *pending.get(&k).unwrap_or(&false) {
				return false;
			}
			pending.insert(k.clone(), true);
		}
		self
			.inflight
			.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		if self.tx.try_send(t).is_err() {
			self
				.inflight
				.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
			// Roll back the pending marker too — else a full-channel failure flags
			// this key forever and dedup blocks every future re-enqueue.
			self.pending.lock().remove(&k);
			return false;
		}
		true
	}

	pub fn dequeued(&self, t: &Task) {
		let k = key_of(t);
		self.pending.lock().remove(&k);
	}

	pub fn done(&self) {
		self
			.inflight
			.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
	}

	pub fn pending_count(&self) -> usize {
		self.pending.lock().len()
	}

	pub fn record_task_latency(&self, d: Duration) {
		let mut s = self.stats.lock();
		s.0 += 1;
		s.1 += d;
	}

	pub fn metrics(&self) -> (i64, i64) {
		let (count, total) = *self.stats.lock();
		let avg = if count > 0 {
			total.as_millis() as i64 / count
		} else {
			0
		};
		(count, avg)
	}

	pub fn record_task_panic(&self, t: &Task, message: &str) {
		record(&self.panics, t, message);
	}

	pub fn panics(&self) -> (u64, Option<TaskFault>) {
		self.panics.lock().clone()
	}

	// A task that returned instead of dying: it re-enqueues every tick, so an
	// unbounded repeat is only visible as a climbing count.
	pub fn record_task_failure(&self, t: &Task, message: &str) {
		record(&self.failures, t, message);
	}

	pub fn failures(&self) -> (u64, Option<TaskFault>) {
		self.failures.lock().clone()
	}
}

fn record(slot: &Mutex<(u64, Option<TaskFault>)>, t: &Task, message: &str) {
	let mut s = slot.lock();
	s.0 += 1;
	s.1 = Some(TaskFault {
		kind: t.kind,
		kern_id: t.kern_id.clone(),
		message: message.to_string(),
	});
}

pub fn task(kind: TaskKind, kern_id: &str) -> Task {
	Task {
		kind,
		kern_id: kern_id.to_string(),
		extra: String::new(),
	}
}

pub fn task_extra(kind: TaskKind, kern_id: &str, extra: &str) -> Task {
	Task {
		kind,
		kern_id: kern_id.to_string(),
		extra: extra.to_string(),
	}
}

// ids newline-joined in `extra`; entity ids never contain a newline, so it round-trips.
pub fn task_commit_access(ids: &[String]) -> Task {
	Task {
		kind: TaskKind::CommitAccess,
		kern_id: String::new(),
		extra: ids.join("\n"),
	}
}

#[cfg(test)]
#[path = "tests/tick_queue_test.rs"]
mod tick_queue_tests;
