use std::path::Path;

use tokio::sync::mpsc;

use crate::watcher_event::{WatchEvent, WatchKind};

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
mod tests {
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
