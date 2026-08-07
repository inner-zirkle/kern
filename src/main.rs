//! The `kern` binary: parse the CLI, capture the launch dir before re-pinning
//! cwd to the project root, and dispatch into `commands`.

use clap::Parser;

use kern::commands::{dispatch, run_server, Cli, Commands};
use kern::config::Config;

// sysexits(3) EX_CONFIG: distinguishes "your settings are wrong" from a crash.
const EXIT_CONFIG: i32 = 78;

fn worker_thread_count(available: Option<usize>) -> usize {
	available.unwrap_or(4).max(4)
}

/// Booting with settings known to be wrong is not failing open, it is failing
/// silently. An absent config is legitimate — `load` already defaults it — so
/// every error it does return is a real one, and every one of them is fatal.
fn boot_config(loaded: Result<Config, kern::config::Error>) -> Result<Config, String> {
	let cfg = loaded.map_err(|e| {
		format!("kern: cannot read the config: {e}\n  fix .kern/kern.toml (or the user-level kern.toml); deleting it is also valid — an absent config uses defaults")
	})?;
	cfg.validate()
		.map_err(|e| format!("kern: invalid config: {e}\n  fix that key in .kern/kern.toml (or the user-level kern.toml) and retry"))?;
	for w in cfg.egress_warnings() {
		tracing::warn!(target: "kern", "{w}");
	}
	for w in cfg.native_knob_warnings() {
		tracing::warn!(target: "kern", "{w}");
	}
	for w in cfg.wsl_loopback_warnings() {
		tracing::warn!(target: "kern", "{w}");
	}
	Ok(cfg)
}

fn main() {
	// Stderr, never stdout: `kern mcp --mcp-stdio` speaks JSON-RPC on stdout.
	let _ = tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
		)
		.with_writer(std::io::stderr)
		.try_init();

	// Floor of 4: the blocking bridges (tick distill, ingest embed, keepalive)
	// each pin a worker; fewer workers starves the time driver and wedges the hub.
	let workers = worker_thread_count(std::thread::available_parallelism().map(|n| n.get()).ok());
	let rt = tokio::runtime::Builder::new_multi_thread()
		.worker_threads(workers)
		.enable_all()
		.build()
		.expect("build tokio runtime");

	rt.block_on(async {
		// Pin to the project root (nearest ancestor with `.kern`): a subdir launch
		// would boot an empty graph while still serving queries.
		let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
		// Before the re-pin: a relative path the caller typed means what it meant
		// where they typed it, so `launch_dir_join` needs the pre-pin cwd.
		kern::set_launch_dir(cwd.clone());
		let root = Config::resolve_root(&cwd);
		if root != cwd {
			tracing::info!(
				target: "kern",
				from = %cwd.display(),
				to = %root.display(),
				"re-pinned cwd to project root (nearest ancestor with .kern)"
			);
			let _ = std::env::set_current_dir(&root);
		}
		// Parse first so `--help`/`--version` still answer in a repo whose config is broken.
		let cli = Cli::parse();
		let cfg = match boot_config(Config::load(&root)) {
			Ok(cfg) => cfg,
			Err(msg) => {
				eprintln!("{msg}");
				std::process::exit(EXIT_CONFIG);
			}
		};

		match cli.command {
			Some(Commands::Daemon) => run_server(&cli, &cfg).await,
			Some(cmd) => dispatch(cmd, &cfg).await,
			None => run_server(&cli, &cfg).await,
		}
	});
}

#[cfg(test)]
mod tests {
	use super::{boot_config, worker_thread_count, Config};

	#[test]
	fn worker_count_honors_the_floor_of_four() {
		assert_eq!(worker_thread_count(None), 4, "detection failure -> floor");
		assert_eq!(worker_thread_count(Some(1)), 4, "1 core -> floor");
		assert_eq!(worker_thread_count(Some(2)), 4);
		assert_eq!(worker_thread_count(Some(4)), 4, "at the floor");
		assert_eq!(worker_thread_count(Some(8)), 8);
		assert_eq!(worker_thread_count(Some(64)), 64);
	}

	#[test]
	fn an_absent_config_still_boots_on_defaults() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let absent_user = root.join("no-such-user-config.toml");

		let cfg = boot_config(Config::load_with_user(&absent_user, &root))
			.expect("no config file is legitimate");
		assert_eq!(
			std::path::PathBuf::from(&cfg.data_dir),
			root.join(".kern").join("data")
		);
	}

	#[test]
	fn a_config_that_fails_validation_is_fatal_and_names_the_key() {
		let mut cfg = Config::default_in(std::path::Path::new("/proj"));
		cfg.embed.url = String::new();

		let err = boot_config(Ok(cfg)).expect_err("an invalid config must not boot");
		assert!(
			err.contains("embed.url"),
			"the offending key is named: {err}"
		);
		assert!(
			err.contains("kern.toml"),
			"the message says where to fix it: {err}"
		);
	}

	#[test]
	fn an_unparseable_config_is_fatal_rather_than_defaulting() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		let kern = root.join(".kern");
		std::fs::create_dir_all(&kern).unwrap();
		std::fs::write(kern.join("kern.toml"), "this is not = = toml\n").unwrap();
		let absent_user = root.join("no-such-user-config.toml");

		let err = boot_config(Config::load_with_user(&absent_user, &root))
			.expect_err("a broken config must not masquerade as an absent one");
		assert!(err.contains("cannot read the config"), "{err}");
	}
}
