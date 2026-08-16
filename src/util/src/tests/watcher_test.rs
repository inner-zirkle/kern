//! Tests extracted from watcher.rs
#![allow(unused)]
use super::*;

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
mod ignore_tests {
	use super::*;
	use tempfile::tempdir;

	#[test]
	fn dot_git_paths_are_always_ignored() {
		let r = IgnoreRules::empty();
		assert!(r.is_ignored(Path::new("/repo/.git/HEAD")));
		assert!(r.is_ignored(Path::new("/repo/sub/.git/index")));
		assert!(!r.is_ignored(Path::new("/repo/src/main.rs")));
	}

	#[test]
	fn gitignore_patterns_match_relative_to_root() {
		let dir = tempdir().unwrap();
		std::fs::write(dir.path().join(".gitignore"), "*.log\ntarget\n").unwrap();
		let rules = IgnoreRules::from_roots(&[dir.path().to_path_buf()]);
		assert!(
			rules.is_ignored(&dir.path().join("server.log")),
			"*.log ignored"
		);
		assert!(
			rules.is_ignored(&dir.path().join("target")),
			"named path ignored"
		);
		assert!(
			!rules.is_ignored(&dir.path().join("src/main.rs")),
			"source kept"
		);
	}

	#[test]
	fn kernignore_rules_are_honored_alongside_gitignore() {
		let dir = tempdir().unwrap();
		std::fs::write(dir.path().join(".kernignore"), "secret*\n").unwrap();
		let rules = IgnoreRules::from_roots(&[dir.path().to_path_buf()]);
		assert!(
			rules.is_ignored(&dir.path().join("secret.txt")),
			".kernignore pattern matches"
		);
		assert!(!rules.is_ignored(&dir.path().join("public.txt")));
	}

	// The self-referential edge: kern parks a watcher record inside its own
	// intake, which lives under the default watched root. Without this the
	// watcher ingests what it just wrote, parks a payload wrapping that payload,
	// and does it again — measured at 283 files from one seed edit in 60s.
	#[test]
	fn denied_prefixes_are_ignored_even_with_no_ignore_file() {
		let dir = tempdir().unwrap();
		let state = dir.path().join(".kern");
		let rules = IgnoreRules::from_roots(&[dir.path().to_path_buf()])
			.with_denied(vec![state.join("intake"), state.join("data")]);
		assert!(rules.is_ignored(&state.join("intake/direct/abc.json")));
		assert!(rules.is_ignored(&state.join("data/data.mdb")));
		assert!(
			!rules.is_ignored(&state.join("kern.toml")),
			"only the named prefixes, not the whole state dir"
		);
		assert!(!rules.is_ignored(&dir.path().join("src/main.rs")));
	}

	#[test]
	fn a_denied_prefix_matches_on_whole_components_not_string_prefix() {
		let dir = tempdir().unwrap();
		let rules = IgnoreRules::empty().with_denied(vec![dir.path().join(".kern").join("intake")]);
		assert!(
			!rules.is_ignored(&dir.path().join(".kern").join("intake-notes.md")),
			"`intake-notes.md` is a sibling of the denied dir, not inside it"
		);
	}

	#[test]
	fn paths_outside_any_root_are_not_ignored() {
		let dir = tempdir().unwrap();
		std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
		let rules = IgnoreRules::from_roots(&[dir.path().to_path_buf()]);
		assert!(!rules.is_ignored(Path::new("/elsewhere/server.log")));
	}

	#[test]
	fn empty_rules_ignore_nothing_except_dot_git() {
		let r = IgnoreRules::empty();
		assert!(!r.is_ignored(Path::new("/anything/file.log")));
		assert!(r.is_ignored(Path::new("/anything/.git/config")));
	}
}
mod pipeline_tests {
	use super::*;
	use std::path::PathBuf;
	use std::time::SystemTime;

	// Paths below must not exist on disk: `canonicalize` fails so the deterministic
	// string-normalisation fallback runs identically on every machine.

	#[test]
	fn file_uri_unix_absolute_path_gets_three_slashes() {
		assert_eq!(
			file_uri(Path::new("/nonexistent_kern_test/dir/file.rs")),
			"file:///nonexistent_kern_test/dir/file.rs"
		);
	}

	#[test]
	fn file_uri_strips_windows_unc_prefix() {
		// Backslashes are literal chars on Unix, so this Windows-shaped input
		// exercises the same string ops on every platform.
		assert_eq!(
			file_uri(Path::new(r"\\?\C:\foo\bar.rs")),
			"file:///C:/foo/bar.rs"
		);
	}

	#[cfg(unix)]
	fn non_utf8_path() -> PathBuf {
		use std::os::unix::ffi::OsStrExt;
		// 0x80 is an invalid UTF-8 lead byte.
		std::ffi::OsStr::from_bytes(&[0x66, 0x80, 0x66]).into()
	}

	#[cfg(windows)]
	fn non_utf8_path() -> PathBuf {
		use std::os::windows::ffi::OsStringExt;
		// 0xD800 is an unpaired surrogate -> not valid UTF-16/UTF-8.
		std::ffi::OsString::from_wide(&[0x66, 0xD800, 0x66]).into()
	}

	#[tokio::test]
	async fn renamed_with_non_utf8_from_reads_the_to_path() {
		let dir = tempfile::tempdir().unwrap();
		let to = dir.path().join("renamed.rs");
		tokio::fs::write(&to, "fn main() {}").await.unwrap();

		let ev = WatchEvent {
			path: to.clone(),
			kind: WatchKind::Renamed {
				from: non_utf8_path(),
				to: to.clone(),
			},
			ts: SystemTime::now(),
		};
		let rec = build_record(&ev)
			.await
			.expect("record built from the `to` path");
		assert_eq!(rec.content, "fn main() {}");
		assert_eq!(rec.language_hint.as_deref(), Some("rust"));
		assert!(rec.source_uri.starts_with("file://"));
	}

	#[tokio::test]
	async fn deleted_events_build_no_record() {
		let ev = WatchEvent {
			path: PathBuf::from("/whatever.rs"),
			kind: WatchKind::Deleted,
			ts: SystemTime::now(),
		};
		assert!(build_record(&ev).await.is_none());
	}
}
mod file_tests {
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
