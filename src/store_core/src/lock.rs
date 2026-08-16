//! The one advisory writer lock over a data dir.
//!
//! LMDB serialises transactions, so two writers never corrupt the file — they
//! corrupt each other's *intent*. Each holds a whole graph in memory and flushes
//! it wholesale, so the loser's version wins by arriving second. Observed
//! 2026-07-21: `kern reembed` rewrote every vector, and a hub respawned by a
//! surviving `kern mcp` proxy flushed its stale in-memory graph over the top,
//! losing the rewrite and one thought.
//!
//! The flush guard catches that when both sides route through it. This catches
//! the case the guard cannot: a long rewrite that has not flushed yet, against a
//! process that boots believing it owns the graph. Held for a process lifetime,
//! released by the OS on exit — including a kill, which is why the pid inside is
//! informational and the lock state is the file lock itself.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const LOCK_FILE: &str = "writer.lock";

pub fn lock_path(data_dir: &str) -> PathBuf {
	Path::new(data_dir).join(LOCK_FILE)
}

/// Dropping this releases the lock.
#[derive(Debug)]
pub struct WriterLock {
	_file: File,
	path: PathBuf,
}

impl WriterLock {
	pub fn path(&self) -> &Path {
		&self.path
	}
}

#[derive(Debug)]
pub enum LockError {
	/// Another live process holds it. `holder` is what that process wrote about
	/// itself — advisory, and absent if it died mid-write.
	Held {
		holder: Option<String>,
	},
	Io(std::io::Error),
}

impl std::fmt::Display for LockError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			LockError::Held { holder: Some(h) } => {
				write!(f, "another kern writer holds this data dir ({h})")
			}
			LockError::Held { holder: None } => write!(f, "another kern writer holds this data dir"),
			LockError::Io(e) => write!(f, "{e}"),
		}
	}
}

impl From<std::io::Error> for LockError {
	fn from(e: std::io::Error) -> Self {
		LockError::Io(e)
	}
}

/// Take the lock, or report who has it. Never blocks: a writer that waits is a
/// writer that hangs when the holder is a daemon that never exits.
pub fn acquire(data_dir: &str, what: &str) -> Result<WriterLock, LockError> {
	std::fs::create_dir_all(data_dir)?;
	let path = lock_path(data_dir);
	let mut file = OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.open(&path)?;

	if file.try_lock().is_err() {
		return Err(LockError::Held {
			holder: read_holder(&path),
		});
	}

	// Only now: the previous holder's line stays readable until someone actually
	// takes over, so a `status` racing an exit sees a stale-but-true name rather
	// than an empty file.
	file.set_len(0)?;
	file.seek(SeekFrom::Start(0))?;
	let _ = write!(file, "{what} pid {}", std::process::id());
	file.flush()?;
	Ok(WriterLock { _file: file, path })
}

/// Who holds it, or None if free. Takes and drops the lock to find out, so it
/// must never be called by a process already holding one — it would report
/// itself as free on platforms with per-process lock semantics.
pub fn holder(data_dir: &str) -> Option<String> {
	let path = lock_path(data_dir);
	let file = File::open(&path).ok()?;
	match file.try_lock() {
		// Free: we just took it. Drop immediately.
		Ok(()) => {
			drop(file);
			None
		}
		Err(_) => Some(read_holder(&path).unwrap_or_else(|| "unknown writer".into())),
	}
}

fn read_holder(path: &Path) -> Option<String> {
	let mut s = String::new();
	File::open(path).ok()?.read_to_string(&mut s).ok()?;
	let s = s.trim().to_string();
	if s.is_empty() {
		None
	} else {
		Some(s)
	}
}

#[cfg(test)]
#[path = "tests/lock_test.rs"]
mod lock_tests;
