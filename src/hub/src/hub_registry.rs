//! The hub's persistent root registry: every kern this machine has ever
//! resolved, with the last stats its daemon reported. The in-RAM node map
//! dies with the hub process; this file is what lets a restarted hub — and
//! the cross-kern search — see cold projects it is not currently serving.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

const REGISTRY_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RootInfo {
	#[serde(default)]
	pub last_seen_ms: u64,
	#[serde(default)]
	pub entities: u64,
	#[serde(default)]
	pub kerns: u64,
	#[serde(default)]
	pub data_bytes: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct RegistryFile {
	version: u32,
	roots: HashMap<String, RootInfo>,
}

pub struct Registry {
	path: PathBuf,
	inner: Mutex<HashMap<String, RootInfo>>,
}

/// `$XDG_STATE_HOME/kern/hub-roots.json` (or the platform equivalent) — user
/// state, not project state: the registry spans every project on the machine,
/// so it cannot live under any one root's `.kern/`.
pub fn default_registry_path() -> PathBuf {
	let base = std::env::var_os("XDG_STATE_HOME")
		.map(PathBuf::from)
		.filter(|p| p.is_absolute());
	#[cfg(windows)]
	let base = base.or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from));
	let base = base.unwrap_or_else(|| {
		std::env::var_os("HOME")
			.map(PathBuf::from)
			.unwrap_or_else(|| PathBuf::from("."))
			.join(".local")
			.join("state")
	});
	base.join("kern").join("hub-roots.json")
}

impl Registry {
	pub fn open_default() -> Self {
		Self::open(default_registry_path())
	}

	/// Load, tolerating an absent or corrupt file — a registry that refuses to
	/// open would take the whole hub down over a cache.
	pub fn open(path: PathBuf) -> Self {
		let roots = std::fs::read_to_string(&path)
			.ok()
			.and_then(|raw| serde_json::from_str::<RegistryFile>(&raw).ok())
			.map(|f| f.roots)
			.unwrap_or_default();
		Self {
			path,
			inner: Mutex::new(roots),
		}
	}

	pub fn path(&self) -> &Path {
		&self.path
	}

	/// A root was resolved (spawned or adopted): remember it exists.
	pub fn record_seen(&self, root: &Path) {
		let key = root.display().to_string();
		let mut map = self.inner.lock().expect("registry lock");
		map.entry(key).or_default().last_seen_ms = util::now_ms();
		self.save(&map);
	}

	/// A stats harvest from a live daemon's health answer.
	pub fn record_stats(&self, root: &Path, entities: u64, kerns: u64, data_bytes: u64) {
		let key = root.display().to_string();
		let mut map = self.inner.lock().expect("registry lock");
		let info = map.entry(key).or_default();
		info.last_seen_ms = util::now_ms();
		info.entities = entities;
		info.kerns = kerns;
		info.data_bytes = data_bytes;
		self.save(&map);
	}

	/// Drop every root whose directory no longer exists; returns what went.
	pub fn prune_missing(&self) -> Vec<String> {
		let mut map = self.inner.lock().expect("registry lock");
		let gone: Vec<String> = map
			.keys()
			.filter(|r| !Path::new(r.as_str()).is_dir())
			.cloned()
			.collect();
		if gone.is_empty() {
			return gone;
		}
		for r in &gone {
			map.remove(r);
		}
		self.save(&map);
		gone
	}

	pub fn roots(&self) -> Vec<(PathBuf, RootInfo)> {
		self
			.inner
			.lock()
			.expect("registry lock")
			.iter()
			.map(|(r, i)| (PathBuf::from(r), i.clone()))
			.collect()
	}

	// Atomic write (temp + rename) so a crash mid-save never leaves a torn
	// file; a save failure is logged, never fatal — the RAM copy still serves.
	fn save(&self, map: &HashMap<String, RootInfo>) {
		let file = RegistryFile {
			version: REGISTRY_VERSION,
			roots: map.clone(),
		};
		let Ok(json) = serde_json::to_string_pretty(&file) else {
			return;
		};
		if let Some(dir) = self.path.parent() {
			if let Err(e) = std::fs::create_dir_all(dir) {
				tracing::warn!(target: "kern.hub", error = %e, "registry dir");
				return;
			}
		}
		let tmp = self.path.with_extension("json.tmp");
		if let Err(e) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &self.path)) {
			tracing::warn!(target: "kern.hub", error = %e, "registry save");
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn scratch_registry(dir: &Path) -> Registry {
		Registry::open(dir.join("hub-roots.json"))
	}

	#[test]
	fn a_seen_root_survives_a_reopen() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("proj");
		std::fs::create_dir_all(&root).unwrap();

		let reg = scratch_registry(dir.path());
		reg.record_seen(&root);
		drop(reg);

		let reg = scratch_registry(dir.path());
		let roots = reg.roots();
		assert_eq!(roots.len(), 1, "the registry is the hub's memory of roots");
		assert_eq!(roots[0].0, root);
		assert!(roots[0].1.last_seen_ms > 0);
	}

	#[test]
	fn stats_overwrite_and_persist() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("proj");
		std::fs::create_dir_all(&root).unwrap();

		let reg = scratch_registry(dir.path());
		reg.record_stats(&root, 42, 3, 1024);
		drop(reg);

		let reg = scratch_registry(dir.path());
		let (_, info) = &reg.roots()[0];
		assert_eq!(info.entities, 42);
		assert_eq!(info.kerns, 3);
		assert_eq!(info.data_bytes, 1024);
	}

	#[test]
	fn prune_drops_only_roots_whose_directory_vanished() {
		let dir = tempfile::tempdir().unwrap();
		let kept = dir.path().join("kept");
		let gone = dir.path().join("gone");
		std::fs::create_dir_all(&kept).unwrap();
		std::fs::create_dir_all(&gone).unwrap();

		let reg = scratch_registry(dir.path());
		reg.record_seen(&kept);
		reg.record_seen(&gone);
		std::fs::remove_dir_all(&gone).unwrap();

		let pruned = reg.prune_missing();
		assert_eq!(pruned, vec![gone.display().to_string()]);
		let roots = reg.roots();
		assert_eq!(roots.len(), 1);
		assert_eq!(roots[0].0, kept, "a deleted project stops being asked");
	}

	#[test]
	fn a_corrupt_file_opens_empty_rather_than_failing() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("hub-roots.json");
		std::fs::write(&path, "{not json").unwrap();
		let reg = Registry::open(path);
		assert!(reg.roots().is_empty(), "a cache never takes the hub down");
	}
}
