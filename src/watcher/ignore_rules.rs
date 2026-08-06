use std::path::{Path, PathBuf};

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
mod tests {
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
