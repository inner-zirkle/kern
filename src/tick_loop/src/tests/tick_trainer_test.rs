//! Tests extracted from tick_trainer.rs
#![allow(unused)]
use super::*;

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
