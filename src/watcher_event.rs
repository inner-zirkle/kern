use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WatchKind {
	Created,
	Modified,
	Deleted,
	Renamed { from: PathBuf, to: PathBuf },
}

// Invariant: for `Renamed`, `path == to` — build via `WatchEvent::new`, not the fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WatchEvent {
	pub path: PathBuf,
	pub kind: WatchKind,
	pub ts: SystemTime,
}

impl WatchEvent {
	pub fn new(path: PathBuf, kind: WatchKind, ts: SystemTime) -> Self {
		let path = match &kind {
			WatchKind::Renamed { to, .. } => to.clone(),
			_ => path,
		};
		Self { path, kind, ts }
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn renamed_event_path_is_forced_to_the_new_location() {
		let ev = WatchEvent::new(
			PathBuf::from("/old.txt"),
			WatchKind::Renamed {
				from: "/old.txt".into(),
				to: "/new.txt".into(),
			},
			SystemTime::UNIX_EPOCH,
		);
		assert_eq!(
			ev.path,
			PathBuf::from("/new.txt"),
			"Renamed path is the new location"
		);
		match ev.kind {
			WatchKind::Renamed { from, to } => {
				assert_eq!(from, PathBuf::from("/old.txt"));
				assert_eq!(to, PathBuf::from("/new.txt"));
			}
			other => panic!("kind preserved, got {other:?}"),
		}
	}

	#[test]
	fn non_renamed_event_keeps_its_given_path() {
		let ev = WatchEvent::new(
			PathBuf::from("/a.rs"),
			WatchKind::Modified,
			SystemTime::UNIX_EPOCH,
		);
		assert_eq!(ev.path, PathBuf::from("/a.rs"));
	}

	#[test]
	fn watch_event_works_as_a_hash_set_key() {
		use std::collections::HashSet;
		let a = WatchEvent::new(
			PathBuf::from("/a"),
			WatchKind::Created,
			SystemTime::UNIX_EPOCH,
		);
		let mut set = HashSet::new();
		set.insert(a.clone());
		assert!(
			set.contains(&a),
			"Hash derive lets WatchEvent be a set/map key"
		);
	}
}
