//! File watching: debounced notify events, filtered by each root's own
//! .gitignore (with `.git` always skipped), fed through the size-capped
//! pipeline into the sink `ingest_file_watcher` adapts.

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

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

pub struct IgnoreRules {
	per_root: Vec<RootRules>,
	// Directory prefixes the host declares off-limits whatever any ignore file
	// says. This crate must not know what they are — the host passes them, so a
	// daemon that writes state inside a watched root can name its own state
	// without this crate depending on it.
	denied: Vec<PathBuf>,
}

struct RootRules {
	root: PathBuf,
	gitignore: Option<Gitignore>,
	kernignore: Option<Gitignore>,
}

impl IgnoreRules {
	pub fn from_roots(roots: &[PathBuf]) -> Self {
		let per_root = roots
			.iter()
			.map(|r| {
				let root = r.clone();
				let gitignore = build(&root, ".gitignore");
				let kernignore = build(&root, ".kernignore");
				RootRules {
					root,
					gitignore,
					kernignore,
				}
			})
			.collect();
		Self {
			per_root,
			denied: Vec::new(),
		}
	}

	// Off-limits prefixes, absolute and in the same coordinate system as the
	// roots. Not an ignore *pattern*: a `.gitignore` is the user's opinion about
	// their own files and can be edited away, while these are directories the
	// host writes into and must never read back.
	pub fn with_denied(mut self, denied: Vec<PathBuf>) -> Self {
		self.denied = denied;
		self
	}

	pub fn empty() -> Self {
		Self {
			per_root: Vec::new(),
			denied: Vec::new(),
		}
	}

	// `matched(rel, false)`: notify event paths are never directory listings, so `is_dir` is always false.
	pub fn is_ignored(&self, path: &Path) -> bool {
		// `.git` always skipped — bursty internal churn, never removed even if unignored.
		if path.components().any(|c| c.as_os_str() == ".git") {
			return true;
		}
		if self.denied.iter().any(|d| path.starts_with(d)) {
			return true;
		}
		for rules in &self.per_root {
			let Ok(rel) = path.strip_prefix(&rules.root) else {
				continue;
			};
			if let Some(g) = &rules.gitignore {
				if g.matched(rel, false).is_ignore() {
					return true;
				}
			}
			if let Some(g) = &rules.kernignore {
				if g.matched(rel, false).is_ignore() {
					return true;
				}
			}
		}
		false
	}
}

fn build(root: &Path, file: &str) -> Option<Gitignore> {
	let path = root.join(file);
	if !path.is_file() {
		return None;
	}
	let mut b = GitignoreBuilder::new(root);
	if b.add(&path).is_some() {
		// `add` returns `Some(error)` on failure (not success); treat as no rules.
		return None;
	}
	b.build().ok()
}

#[cfg(test)]
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

use tokio::sync::mpsc;

pub const MAX_INGEST_BYTES: u64 = 1024 * 1024;

// `source_uri` must be a `file://` URI — kern's `ingest` MCP tool requires that scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestRecord {
	pub source_uri: String,
	pub content: String,
	pub language_hint: Option<String>,
	/// The source_uri the new file replaces, on a `Renamed` event. The sink
	/// supersedes the old-path entity so a move-plus-edit does not leave a
	/// dangling stale `Document` beside the new one. `None` for ordinary events.
	pub replaces: Option<String>,
}

// This crate must NOT depend on kern; the sink is implemented by the kern wiring.
#[async_trait::async_trait]
pub trait IngestSink: Send + Sync + 'static {
	async fn ingest(&self, record: IngestRecord);
}

// `Deleted` is intentionally ignored here — kern deletes via a separate path.
pub struct IngestPipeline<S: IngestSink> {
	sink: S,
}

impl<S: IngestSink> IngestPipeline<S> {
	pub fn new(sink: S) -> Self {
		Self { sink }
	}

	pub async fn run(self, mut rx: mpsc::UnboundedReceiver<WatchEvent>) {
		while let Some(ev) = rx.recv().await {
			if let Some(rec) = build_record(&ev).await {
				self.sink.ingest(rec).await;
			}
		}
	}

	pub async fn handle(&self, ev: WatchEvent) {
		if let Some(rec) = build_record(&ev).await {
			self.sink.ingest(rec).await;
		}
	}
}

async fn build_record(ev: &WatchEvent) -> Option<IngestRecord> {
	let (path, replaces): (&Path, Option<String>) = match &ev.kind {
		WatchKind::Created | WatchKind::Modified => (&ev.path, None),
		// `WatchEvent::new` forces `ev.path` to `to`, so the old path comes from
		// the `from` half of the kind — `ev.path` would be the new location.
		WatchKind::Renamed { from, to } => (to, Some(file_uri(from))),
		WatchKind::Deleted => return None,
	};

	let meta = tokio::fs::metadata(path).await.ok()?;
	if !meta.is_file() {
		return None;
	}
	if meta.len() > MAX_INGEST_BYTES {
		tracing::debug!(?path, size = meta.len(), "skipping oversize file");
		return None;
	}
	let bytes = tokio::fs::read(path).await.ok()?;
	let content = match String::from_utf8(bytes) {
		Ok(s) => s,
		Err(_) => {
			tracing::debug!(?path, "skipping non-utf8 file");
			return None;
		}
	};

	Some(IngestRecord {
		source_uri: file_uri(path),
		content,
		language_hint: language_hint(path),
		replaces,
	})
}

fn file_uri(path: &Path) -> String {
	let abs = match path.canonicalize() {
		Ok(p) => p,
		Err(_) => path.to_path_buf(),
	};
	let s = abs.to_string_lossy().replace('\\', "/");
	// Windows canonicalize returns `\\?\C:\foo` (now `//?/C:/foo`); strip the UNC prefix.
	let trimmed = s.strip_prefix("//?/").unwrap_or(&s);
	if trimmed.starts_with('/') {
		format!("file://{trimmed}")
	} else {
		format!("file:///{trimmed}")
	}
}

fn language_hint(path: &Path) -> Option<String> {
	let ext = path.extension()?.to_str()?.to_ascii_lowercase();
	let hint = match ext.as_str() {
		"rs" => "rust",
		"ts" | "tsx" => "typescript",
		"js" | "jsx" | "mjs" | "cjs" => "javascript",
		"py" => "python",
		"go" => "go",
		"md" => "markdown",
		"toml" => "toml",
		"json" => "json",
		"yaml" | "yml" => "yaml",
		_ => return Some(ext),
	};
	Some(hint.to_string())
}

#[cfg(test)]
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

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::task::JoinHandle;

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
