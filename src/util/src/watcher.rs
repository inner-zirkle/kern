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
#[path = "tests/watcher_test.rs"]
mod watcher_tests;

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
				// Canonicalize the root once, up front: FSEvents reports every
				// path in canonical form (macOS `/var` is a symlink to
				// `/private/var`), and every later path comparison — the
				// `strip_prefix` in `is_ignored` included — happens in the
				// event path's coordinate system. A root stored as given would
				// silently never match its own subtree, so its gitignore rules
				// would never apply. The root exists at build time, so
				// canonicalize cannot fail for a legitimate root.
				let root = r.canonicalize().unwrap_or_else(|_| r.clone());
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
		// Same canonicalization argument as `from_roots`: the host names its own
		// state dirs in the paths it knows, which may differ from the canonical
		// form notify reports events in.
		self.denied = denied
			.into_iter()
			.map(|d| d.canonicalize().unwrap_or(d))
			.collect();
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
		// The event path may come in a different coordinate system than the
		// canonical roots (FSEvents always reports `/private/var/...` even when
		// the root was registered as `/var/...`). Canonicalizing the *parent*
		// (which always exists) and re-appending the file name brings the event
		// into the same system as the roots; canonicalizing the file itself
		// would fail for `Deleted` events, whose victim is already gone.
		let resolved = match (path.parent(), path.file_name()) {
			(Some(parent), Some(name)) => parent
				.canonicalize()
				.map(|p| p.join(name))
				.unwrap_or_else(|_| path.to_path_buf()),
			_ => path.to_path_buf(),
		};
		let path: &Path = &resolved;
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
