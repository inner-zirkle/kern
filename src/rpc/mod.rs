pub mod kern_rpc_server;

pub use kern_rpc_server::{serve_kern_rpc_loop, KernRpcHandler};

use std::path::{Path, PathBuf};

use crate::transport::kern_rpc::AuthReq;

/// The secret a client presents to the daemon rooted at `root`.
///
/// Two lookups, and the second one is the whole point: the *socket* is keyed by
/// the root (`Endpoint::kern_for` hashes the path) while the *token* is keyed by
/// the data_dir. Those diverge the moment kern.toml is repointed under a live
/// daemon — the daemon keeps serving out of the store it opened at boot, and a
/// later CLI, reading the new config, would go looking for the secret in a
/// directory that daemon never wrote to. So: the configured store first, the
/// root's conventional `.kern/data` second.
///
/// Searching costs nothing to be wrong about. The daemon compares against the
/// secret *it* resolved, so the only outcome of guessing badly is a refusal —
/// this is a client hunting for a key, never a server deciding to accept one.
fn token_for(root: &Path, cfg: &crate::config::Config) -> Option<String> {
	cfg
		.serve
		.read_mcp_token(Path::new(&cfg.data_dir))
		.or_else(|| {
			let conventional = crate::config::Config::default_in(root).data_dir;
			(conventional != cfg.data_dir)
				.then(|| cfg.serve.read_mcp_token(Path::new(&conventional)))
				.flatten()
		})
}

fn cwd() -> PathBuf {
	std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// What a client presents on connect: the graph's `mcp-token`. For the commands
/// that run pinned to the project root, which is all of them — `main.rs` re-pins
/// cwd before dispatch.
///
/// No token found leaves it empty, and the daemon refuses an empty token like
/// any other wrong one: a caller that cannot prove itself is turned away, not
/// waved through.
pub fn caller_of(cfg: &crate::config::Config) -> AuthReq {
	AuthReq::new(token_for(&cwd(), cfg).unwrap_or_default())
}

/// The same, for a caller that knows only which directory the graph lives in —
/// the hub, whose nodes are addressed by a root it never stands in.
pub fn caller_at(root: &Path) -> AuthReq {
	let cfg =
		crate::config::Config::load(root).unwrap_or_else(|_| crate::config::Config::default_in(root));
	AuthReq::new(token_for(root, &cfg).unwrap_or_default())
}

/// The caller identity for the graph rooted at this process's cwd — the root
/// `Endpoint::kern()` resolves against.
pub fn caller() -> AuthReq {
	caller_at(&cwd())
}

#[cfg(test)]
mod caller_tests {
	use super::*;
	use crate::config::mcp_token_path;

	fn write_token(data_dir: &Path, token: &str) {
		std::fs::create_dir_all(data_dir).unwrap();
		std::fs::write(mcp_token_path(data_dir), token).unwrap();
	}

	#[test]
	fn the_configured_store_is_where_the_token_is_read_from() {
		let root = tempfile::tempdir().unwrap();
		let mut cfg = crate::config::Config::default_in(root.path());
		cfg.data_dir = root.path().join("store").to_string_lossy().into_owned();
		write_token(Path::new(&cfg.data_dir), "configured-secret");
		assert_eq!(
			token_for(root.path(), &cfg).as_deref(),
			Some("configured-secret")
		);
	}

	// The e2e blinding shape, and the reason the fallback exists: the daemon
	// booted on the conventional store, then kern.toml was repointed. The socket
	// is still the root's, so the caller must still find the root's secret.
	#[test]
	fn a_repointed_data_dir_still_finds_the_secret_the_root_daemon_minted() {
		let root = tempfile::tempdir().unwrap();
		let conventional = crate::config::Config::default_in(root.path()).data_dir;
		write_token(Path::new(&conventional), "the-daemons-secret");

		let mut cfg = crate::config::Config::default_in(root.path());
		cfg.data_dir = root.path().join("blind").to_string_lossy().into_owned();
		assert_eq!(
			token_for(root.path(), &cfg).as_deref(),
			Some("the-daemons-secret"),
			"a blinded CLI must still reach the daemon it is supposed to route to"
		);
	}

	#[test]
	fn no_token_anywhere_yields_none_rather_than_something_forgiving() {
		let root = tempfile::tempdir().unwrap();
		let cfg = crate::config::Config::default_in(root.path());
		assert!(token_for(root.path(), &cfg).is_none());
		assert!(
			caller_at(root.path()).token.is_empty(),
			"an absent secret presents as empty, which verify_auth refuses"
		);
	}

	// A file that exists but says nothing is the same as no file. Neither a
	// zero-length token nor one that is only whitespace may become the empty
	// string a caller then presents — `verify_auth` refuses an empty `expected`,
	// but an empty *offered* token against a real secret must be refused by the
	// compare, and the cheapest way to be sure is never to mint one.
	#[test]
	fn a_blank_token_file_reads_as_no_token_at_all() {
		for body in ["", "   ", "\n", " \t\r\n "] {
			let root = tempfile::tempdir().unwrap();
			let cfg = crate::config::Config::default_in(root.path());
			write_token(Path::new(&cfg.data_dir), body);
			assert!(
				token_for(root.path(), &cfg).is_none(),
				"a blank mcp-token must not present as a token ({body:?})"
			);
			assert!(caller_at(root.path()).token.is_empty());
		}
	}

	#[test]
	fn an_explicit_configured_token_wins_and_needs_no_file() {
		let root = tempfile::tempdir().unwrap();
		let mut cfg = crate::config::Config::default_in(root.path());
		cfg.serve.mcp_token = "explicit".into();
		assert_eq!(token_for(root.path(), &cfg).as_deref(), Some("explicit"));
	}
}
