use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServeConfig {
	// Empty = read (or mint) the token file instead; see `resolve_mcp_token`.
	pub mcp_token: String,
	// Empty = no MCP-over-HTTP listener. `--mcp-addr` overrides it.
	pub mcp_addr: String,
}

pub fn mcp_token_path(data_dir: &Path) -> PathBuf {
	data_dir.join("mcp-token")
}

fn mint_token() -> String {
	use rand::RngExt;
	let mut rng = rand::rng();
	format!(
		"{:016x}{:016x}{:016x}{:016x}",
		rng.random::<u64>(),
		rng.random::<u64>(),
		rng.random::<u64>(),
		rng.random::<u64>()
	)
}

// Owner-only from the moment the file exists, so content is never briefly world-readable.
#[cfg(unix)]
fn private_opts() -> std::fs::OpenOptions {
	use std::os::unix::fs::OpenOptionsExt;
	let mut o = std::fs::OpenOptions::new();
	o.mode(0o600);
	o
}

#[cfg(not(unix))]
fn private_opts() -> std::fs::OpenOptions {
	std::fs::OpenOptions::new()
}

fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
	private_opts().write(true).create_new(true).open(path)
}

/// Append-open (creating if absent), owner-only. `mode` applies only on creation,
/// so an already-loose file is re-tightened; a chmod that the filesystem refuses
/// must not cost us the handle.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
	let f = private_opts().append(true).create(true).open(path)?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
	}
	Ok(f)
}

impl ServeConfig {
	/// The token the HTTP/SSE surface must demand. An explicit `mcp_token` wins;
	/// otherwise the per-graph token file is read, minting it on first use so a
	/// local user never has to configure anything.
	pub fn resolve_mcp_token(&self, data_dir: &Path) -> std::io::Result<String> {
		if !self.mcp_token.is_empty() {
			return Ok(self.mcp_token.clone());
		}
		let path = mcp_token_path(data_dir);
		match std::fs::read_to_string(&path) {
			Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_string()),
			Ok(_) => {
				let _ = std::fs::remove_file(&path);
			}
			Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
			Err(_) => {}
		}
		if let Some(parent) = path.parent() {
			std::fs::create_dir_all(parent)?;
		}
		let token = mint_token();
		match create_private(&path) {
			Ok(mut f) => {
				use std::io::Write;
				f.write_all(token.as_bytes())?;
				Ok(token)
			}
			// Lost the create race to a sibling process: its token is the real one.
			Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
				Ok(std::fs::read_to_string(&path)?.trim().to_string())
			}
			Err(e) => Err(e),
		}
	}

	/// Read-only twin of `resolve_mcp_token`, for the *callers* of kern.sock.
	/// A client must be able to present the daemon's secret, never to invent
	/// one: minting here would drop an `mcp-token` into every directory a CLI
	/// is run in, and — the part that matters — a client that mints its own
	/// token is a client authenticating against nothing.
	///
	/// `None` means "no secret to present", which the daemon refuses. That is
	/// the right answer: whenever a daemon is listening it has already minted
	/// the file, so an absent one means nothing is there to talk to anyway.
	pub fn read_mcp_token(&self, data_dir: &Path) -> Option<String> {
		if !self.mcp_token.is_empty() {
			return Some(self.mcp_token.clone());
		}
		std::fs::read_to_string(mcp_token_path(data_dir))
			.ok()
			.map(|t| t.trim().to_string())
			.filter(|t| !t.is_empty())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn an_explicit_token_wins_over_the_file() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = ServeConfig {
			mcp_token: "configured".into(),
			..Default::default()
		};
		assert_eq!(cfg.resolve_mcp_token(dir.path()).unwrap(), "configured");
		assert!(
			!mcp_token_path(dir.path()).exists(),
			"an explicit token mints no file"
		);
	}

	#[test]
	fn a_token_is_minted_once_and_then_reused() {
		let dir = tempfile::tempdir().unwrap();
		let cfg = ServeConfig::default();
		let first = cfg.resolve_mcp_token(dir.path()).unwrap();
		assert_eq!(first.len(), 64, "256 bits of hex");
		assert_eq!(
			cfg.resolve_mcp_token(dir.path()).unwrap(),
			first,
			"a second resolve reuses the minted token"
		);
	}

	#[test]
	fn mcp_addr_is_off_by_default_and_reads_from_toml() {
		assert!(
			ServeConfig::default().mcp_addr.is_empty(),
			"no HTTP listener unless asked for"
		);
		let cfg: ServeConfig = toml::from_str("mcp_addr = \"127.0.0.1:7777\"\n").unwrap();
		assert_eq!(cfg.mcp_addr, "127.0.0.1:7777");
		assert!(
			cfg.mcp_token.is_empty(),
			"the other field keeps its default"
		);
	}

	#[test]
	fn open_private_append_creates_then_appends_without_truncating() {
		let dir = tempfile::tempdir().unwrap();
		let p = dir.path().join("d.log");

		{
			use std::io::Write;
			let mut f = open_private_append(&p).expect("create");
			f.write_all(b"first\n").unwrap();
		}
		{
			use std::io::Write;
			let mut f = open_private_append(&p).expect("reopen");
			f.write_all(b"second\n").unwrap();
		}

		assert_eq!(
			std::fs::read_to_string(&p).unwrap(),
			"first\nsecond\n",
			"a reopen must not erase what explains the restart"
		);
	}

	#[cfg(unix)]
	#[test]
	fn open_private_append_tightens_a_world_readable_file() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let p = dir.path().join("loose.log");
		std::fs::write(&p, "x").unwrap();
		std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

		open_private_append(&p).expect("open");

		let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
		assert_eq!(
			mode, 0o600,
			"captured text must not stay readable: {mode:o}"
		);
	}

	#[cfg(unix)]
	#[test]
	fn the_minted_token_file_is_owner_only() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		ServeConfig::default()
			.resolve_mcp_token(dir.path())
			.unwrap();
		let mode = std::fs::metadata(mcp_token_path(dir.path()))
			.unwrap()
			.permissions()
			.mode()
			& 0o777;
		assert_eq!(
			mode, 0o600,
			"the token must not be world-readable: {mode:o}"
		);
	}
}
