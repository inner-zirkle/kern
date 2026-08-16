//! Tests extracted from tick_queue.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn enqueue_dedups_an_already_pending_key() {
		let q = Queue::new(8);
		assert!(q.enqueue(task(TaskKind::Cluster, "k")));
		assert!(
			!q.enqueue(task(TaskKind::Cluster, "k")),
			"same key is deduped"
		);
		assert_eq!(q.pending_count(), 1);
	}

	#[test]
	fn dequeued_clears_pending_so_the_key_can_re_enqueue() {
		let q = Queue::new(8);
		let t = task(TaskKind::Persist, "k");
		assert!(q.enqueue(t.clone()));
		assert!(!q.enqueue(t.clone()), "still pending -> deduped");
		q.dequeued(&t);
		assert_eq!(q.pending_count(), 0);
		assert!(q.enqueue(t), "re-enqueue succeeds after dequeue");
	}

	#[test]
	fn full_channel_send_failure_rolls_back_pending() {
		let q = Queue::new(1);
		assert!(q.enqueue(task(TaskKind::Cluster, "a")));
		let b = task(TaskKind::Cluster, "b");
		assert!(!q.enqueue(b.clone()), "full channel -> enqueue fails");
		assert_eq!(
			q.pending_count(),
			1,
			"only 'a' remains pending; 'b' was rolled back"
		);
		let mut rx = q.take_receiver().unwrap();
		let _ = rx.try_recv();
		assert!(q.enqueue(b), "b re-enqueues once a slot frees");
	}

	#[test]
	fn record_task_latency_accumulates_count_and_average() {
		let q = Queue::new(8);
		q.record_task_latency(Duration::from_millis(10));
		q.record_task_latency(Duration::from_millis(30));
		let (count, avg_ms) = q.metrics();
		assert_eq!(count, 2);
		assert_eq!(avg_ms, 20, "average latency = (10 + 30) / 2 ms");
	}

	#[test]
	fn a_fresh_queue_reports_no_panics() {
		let q = Queue::new(8);
		let (count, last) = q.panics();
		assert_eq!(count, 0);
		assert!(
			last.is_none(),
			"idle maintenance is not degraded maintenance"
		);
	}

	#[test]
	fn record_task_panic_counts_and_keeps_the_latest() {
		let q = Queue::new(8);
		q.record_task_panic(&task(TaskKind::Cluster, "k1"), "first boom");
		q.record_task_panic(&task(TaskKind::GnnPropagate, "k2"), "second boom");

		let (count, last) = q.panics();
		assert_eq!(count, 2, "every panic counts");
		let last = last.expect("the most recent panic is retained");
		assert_eq!(last.kind, TaskKind::GnnPropagate);
		assert_eq!(last.kern_id, "k2");
		assert_eq!(last.message, "second boom");
		assert_eq!(last.to_string(), "GnnPropagate[k2]: second boom");
	}

	#[test]
	fn failures_count_separately_from_panics() {
		let q = Queue::new(8);
		assert_eq!(q.failures(), (0, None), "a fresh queue has failed nothing");

		q.record_task_failure(&task(TaskKind::GnnPropagate, "k1"), "train epoch 0 forward");
		q.record_task_panic(&task(TaskKind::Cluster, "k2"), "boom");

		let (failed, last) = q.failures();
		assert_eq!(failed, 1, "the contained failure is counted");
		assert_eq!(
			last.expect("retained").to_string(),
			"GnnPropagate[k1]: train epoch 0 forward"
		);
		assert_eq!(
			q.panics().0,
			1,
			"a panic never lands in the failure counter"
		);
	}

	#[test]
	fn a_graph_global_task_panic_renders_without_an_empty_kern_slot() {
		let q = Queue::new(8);
		q.record_task_panic(&task(TaskKind::IdleSweep, ""), "boom");
		let last = q.panics().1.expect("recorded");
		assert_eq!(last.to_string(), "IdleSweep: boom");
	}
}
