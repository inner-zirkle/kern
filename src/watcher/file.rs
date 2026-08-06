use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant, SystemTime};

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::event::{WatchEvent, WatchKind};
use super::ignore_rules::IgnoreRules;

// Debounce window: wide enough to coalesce the multi-event burst Windows notify
// fires per logical edit, short enough for interactive saves. Milliseconds.
const DEBOUNCE: Duration = Duration::from_millis(50);

#[derive(Debug, Error)]
pub enum WatcherError {
	#[error("notify error: {0}")]
	Notify(#[from] notify::Error),
	#[error("watcher event channel closed")]
	Closed,
}

pub struct FileWatcher {
	// Drop order matters: `_notify` must drop before `_task` so the std channel
	// closes and the blocking coalesce loop exits; field order dictates drop order.
	rx: mpsc::UnboundedReceiver<WatchEvent>,
	_notify: RecommendedWatcher,
	_task: JoinHandle<()>,
}

impl FileWatcher {
	pub fn new(roots: Vec<PathBuf>, ignore: IgnoreRules) -> Result<Self, WatcherError> {
		let (raw_tx, raw_rx) = std_mpsc::channel::<notify::Result<Event>>();
		let mut notify_watcher = notify::recommended_watcher(move |res| {
			let _ = raw_tx.send(res);
		})?;

		for root in &roots {
			notify_watcher.watch(root, RecursiveMode::Recursive)?;
		}

		let (out_tx, out_rx) = mpsc::unbounded_channel::<WatchEvent>();
		let task = spawn_coalescer(raw_rx, out_tx, ignore);

		Ok(Self {
			rx: out_rx,
			_notify: notify_watcher,
			_task: task,
		})
	}

	pub async fn next_event(&mut self) -> Option<WatchEvent> {
		self.rx.recv().await
	}
}

fn spawn_coalescer(
	raw_rx: std_mpsc::Receiver<notify::Result<Event>>,
	out_tx: mpsc::UnboundedSender<WatchEvent>,
	ignore: IgnoreRules,
) -> JoinHandle<()> {
	tokio::task::spawn_blocking(move || coalesce_loop(raw_rx, out_tx, ignore))
}

struct Pending {
	event: WatchEvent,
	deadline: Instant,
}

fn coalesce_loop(
	raw_rx: std_mpsc::Receiver<notify::Result<Event>>,
	out_tx: mpsc::UnboundedSender<WatchEvent>,
	ignore: IgnoreRules,
) {
	let mut pending: HashMap<PathBuf, Pending> = HashMap::new();

	loop {
		let timeout = next_timeout(&pending);
		let recv = match timeout {
			Some(t) => raw_rx.recv_timeout(t),
			None => match raw_rx.recv() {
				Ok(v) => Ok(v),
				Err(_) => Err(std_mpsc::RecvTimeoutError::Disconnected),
			},
		};

		match recv {
			Ok(Ok(ev)) => {
				for we in translate(ev, &ignore) {
					let key = we.path.clone();
					pending.insert(
						key,
						Pending {
							event: we,
							deadline: Instant::now() + DEBOUNCE,
						},
					);
				}
			}
			Ok(Err(err)) => {
				tracing::warn!(?err, "notify error");
			}
			Err(std_mpsc::RecvTimeoutError::Timeout) => {}
			Err(std_mpsc::RecvTimeoutError::Disconnected) => {
				flush_all(&mut pending, &out_tx);
				return;
			}
		}

		flush_due(&mut pending, &out_tx);
		if out_tx.is_closed() {
			return;
		}
	}
}

fn next_timeout(pending: &HashMap<PathBuf, Pending>) -> Option<Duration> {
	let earliest = pending.values().map(|p| p.deadline).min()?;
	let now = Instant::now();
	Some(earliest.saturating_duration_since(now))
}

fn flush_due(pending: &mut HashMap<PathBuf, Pending>, out_tx: &mpsc::UnboundedSender<WatchEvent>) {
	let now = Instant::now();
	let due: Vec<PathBuf> = pending
		.iter()
		.filter(|(_, v)| v.deadline <= now)
		.map(|(k, _)| k.clone())
		.collect();
	for key in due {
		if let Some(p) = pending.remove(&key) {
			let _ = out_tx.send(p.event);
		}
	}
}

fn flush_all(pending: &mut HashMap<PathBuf, Pending>, out_tx: &mpsc::UnboundedSender<WatchEvent>) {
	for (_, p) in pending.drain() {
		let _ = out_tx.send(p.event);
	}
}

fn translate(ev: Event, ignore: &IgnoreRules) -> Vec<WatchEvent> {
	let ts = SystemTime::now();
	let paths = ev.paths;

	let mk = |path: PathBuf, kind: WatchKind| -> Option<WatchEvent> {
		if ignore.is_ignored(&path) {
			return None;
		}
		Some(WatchEvent::new(path, kind, ts))
	};

	match ev.kind {
		EventKind::Create(
			CreateKind::File | CreateKind::Folder | CreateKind::Any | CreateKind::Other,
		) => paths
			.into_iter()
			.filter_map(|p| mk(p, WatchKind::Created))
			.collect(),
		EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
			match <[PathBuf; 2]>::try_from(paths) {
				Ok([from, to]) => {
					if ignore.is_ignored(&to) && ignore.is_ignored(&from) {
						return Vec::new();
					}
					vec![WatchEvent::new(
						to.clone(),
						WatchKind::Renamed { from, to },
						ts,
					)]
				}
				// Some backends deliver `Both` with a single endpoint (or none); degrade to Modified.
				Err(paths) => paths
					.into_iter()
					.filter_map(|p| mk(p, WatchKind::Modified))
					.collect(),
			}
		}
		EventKind::Modify(ModifyKind::Name(RenameMode::From)) => paths
			.into_iter()
			.filter_map(|p| mk(p, WatchKind::Deleted))
			.collect(),
		EventKind::Modify(ModifyKind::Name(RenameMode::To)) => paths
			.into_iter()
			.filter_map(|p| mk(p, WatchKind::Created))
			.collect(),
		EventKind::Modify(_) => paths
			.into_iter()
			.filter_map(|p| mk(p, WatchKind::Modified))
			.collect(),
		EventKind::Remove(
			RemoveKind::File | RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other,
		) => paths
			.into_iter()
			.filter_map(|p| mk(p, WatchKind::Deleted))
			.collect(),
		_ => Vec::new(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn ev(kind: EventKind, paths: &[&str]) -> Event {
		let mut e = Event::new(kind);
		for p in paths {
			e = e.add_path(PathBuf::from(p));
		}
		e
	}

	#[test]
	fn translate_create_file_to_created() {
		let out = translate(
			ev(EventKind::Create(CreateKind::File), &["/a.txt"]),
			&IgnoreRules::empty(),
		);
		assert_eq!(out.len(), 1);
		assert_eq!(out[0].kind, WatchKind::Created);
		assert_eq!(out[0].path, PathBuf::from("/a.txt"));
	}

	#[test]
	fn translate_rename_both_collapses_to_single_renamed() {
		let kind = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
		let out = translate(ev(kind, &["/old.txt", "/new.txt"]), &IgnoreRules::empty());
		assert_eq!(out.len(), 1, "Both -> exactly one Renamed");
		match &out[0].kind {
			WatchKind::Renamed { from, to } => {
				assert_eq!(from, &PathBuf::from("/old.txt"));
				assert_eq!(to, &PathBuf::from("/new.txt"));
			}
			other => panic!("expected Renamed, got {other:?}"),
		}
		assert_eq!(out[0].path, PathBuf::from("/new.txt"));
	}

	#[test]
	fn translate_rename_both_with_wrong_arity_is_not_a_rename() {
		let kind = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
		let out = translate(ev(kind, &["/only.txt"]), &IgnoreRules::empty());
		assert_eq!(out.len(), 1);
		assert_eq!(out[0].kind, WatchKind::Modified);

		let none = translate(ev(kind, &[]), &IgnoreRules::empty());
		assert!(none.is_empty(), "pathless Both event produces nothing");

		let three = translate(
			ev(kind, &["/a.txt", "/b.txt", "/c.txt"]),
			&IgnoreRules::empty(),
		);
		assert_eq!(three.len(), 3);
		assert!(three.iter().all(|e| e.kind == WatchKind::Modified));
	}

	#[test]
	fn translate_rename_half_events_split_to_delete_and_create() {
		let from = translate(
			ev(
				EventKind::Modify(ModifyKind::Name(RenameMode::From)),
				&["/g.txt"],
			),
			&IgnoreRules::empty(),
		);
		assert_eq!(from[0].kind, WatchKind::Deleted, "From half -> Deleted");
		let to = translate(
			ev(
				EventKind::Modify(ModifyKind::Name(RenameMode::To)),
				&["/h.txt"],
			),
			&IgnoreRules::empty(),
		);
		assert_eq!(to[0].kind, WatchKind::Created, "To half -> Created");
	}

	#[test]
	fn translate_generic_modify_and_remove_map_to_expected_kinds() {
		let m = translate(
			ev(EventKind::Modify(ModifyKind::Any), &["/m.txt"]),
			&IgnoreRules::empty(),
		);
		assert_eq!(m[0].kind, WatchKind::Modified);
		let r = translate(
			ev(EventKind::Remove(RemoveKind::File), &["/r.txt"]),
			&IgnoreRules::empty(),
		);
		assert_eq!(r[0].kind, WatchKind::Deleted);
	}

	#[test]
	fn translate_non_actionable_access_events_are_dropped() {
		let out = translate(
			ev(
				EventKind::Access(notify::event::AccessKind::Any),
				&["/a.txt"],
			),
			&IgnoreRules::empty(),
		);
		assert!(out.is_empty(), "Access events produce no WatchEvent");
	}
}
