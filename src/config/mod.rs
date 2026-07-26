pub mod detached_log;
mod embed;
mod gnn;
mod gossip;
mod graph;
mod hub;
mod ingest;
mod intake;
pub mod io;
mod preset;
mod reason;
mod reload;
mod retrieval;
mod secrets;
mod serve;
mod tick;
mod watcher;

pub use embed::{EmbedConfig, DEFAULT_EMBED_MODEL, DEFAULT_EMBED_URL};
pub use gnn::GnnConfig;
pub use gossip::{ContractConfig, GossipConfig};
pub use graph::GraphConfig;
pub use hub::HubConfig;
pub use ingest::IngestConfig;
pub use intake::IntakeConfig;
pub use preset::Preset;
pub use reason::{ReasonConfig, DEFAULT_REASON_TIMEOUT_SECS};
pub use reload::ReloadConfig;
pub use retrieval::RetrievalConfig;
pub use serve::{mcp_token_path, open_private_append, ServeConfig};
pub use tick::TickConfig;
pub use watcher::WatcherConfig;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::base::heat::HeatConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
	pub data_dir: String,
	pub preset: Preset,
	pub embed: EmbedConfig,
	pub reason: ReasonConfig,
	pub serve: ServeConfig,
	pub retrieval: RetrievalConfig,
	pub ingest: IngestConfig,
	pub gossip: GossipConfig,
	pub tick: TickConfig,
	pub heat: HeatConfig,
	pub gnn: GnnConfig,
	pub watcher: WatcherConfig,
	pub intake: IntakeConfig,
	pub graph: GraphConfig,
	pub hub: HubConfig,
	pub reload: ReloadConfig,
}

impl Default for Config {
	fn default() -> Self {
		let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
		Self::default_in(&cwd)
	}
}

// Pin a relative `data_dir` to the load-time `cwd`: re-resolving against the
// live current_dir silently reads an empty graph from a wrong launch dir.
fn graviton_data_dir(data_dir: &str, cwd: &Path) -> String {
	let p = Path::new(data_dir);
	if p.is_absolute() {
		data_dir.to_string()
	} else {
		cwd.join(p).to_string_lossy().into_owned()
	}
}

impl Config {
	pub fn default_in(cwd: &Path) -> Self {
		let mut cfg = Self {
			data_dir: cwd
				.join(".kern")
				.join("data")
				.to_string_lossy()
				.into_owned(),
			preset: Preset::default(),
			embed: EmbedConfig::default(),
			reason: ReasonConfig::default(),
			serve: ServeConfig::default(),
			retrieval: RetrievalConfig::default(),
			ingest: IngestConfig::default(),
			gossip: GossipConfig::default(),
			tick: TickConfig::default(),
			heat: HeatConfig::default(),
			gnn: GnnConfig::default(),
			watcher: WatcherConfig::default(),
			intake: IntakeConfig::default(),
			graph: GraphConfig::default(),
			hub: HubConfig::default(),
			reload: ReloadConfig::default(),
		};
		let preset = cfg.preset;
		preset.apply(&mut cfg);
		cfg
	}

	pub fn load(cwd: &Path) -> Result<Self, io::Error> {
		let user = dirs::config_dir()
			.map(|d| d.join("kern").join("kern.toml"))
			.unwrap_or_else(|| cwd.join(".kern").join("kern.toml"));
		Self::load_with_user(&user, cwd)
	}

	/// `load` with the user scope named explicitly. A test that lets `load`
	/// resolve it reads the developer's real `~/.config/kern/kern.toml` and
	/// passes or fails on whatever happens to be on that machine.
	pub fn load_with_user(user: &Path, cwd: &Path) -> Result<Self, io::Error> {
		let project = cwd.join(".kern").join("kern.toml");
		let merged = io::merged_value(user, &project)?;
		for section in ["heat", "ingest", "retrieval"] {
			let Some(table) = merged.get(section) else {
				continue;
			};
			// `[ingest] review_policy` is the one exception, and it is not a
			// loosening of the rule: what a preset owns is TUNING, and in this table
			// `Preset::apply` writes exactly one key, `dedup_threshold`. Curation
			// policy is not tuning, and refusing the whole table left `review_policy`
			// settable from nowhere outside the process — the same unreachability
			// ROADMAP item 21 records for `exclude_pending`, one layer down. Any
			// other key here is still refused, so the tuning surface is unchanged.
			let only_review_policy = section == "ingest"
				&& table
					.as_table()
					.is_some_and(|t| t.keys().all(|k| k == "review_policy"));
			if !only_review_policy {
				let escape = if section == "ingest" {
					" (the one key it does accept is `review_policy`, which is curation, not tuning)"
				} else {
					""
				};
				return Err(io::Error::Parse(format!(
					"[{section}] is preset-managed — set preset = \"relaxed\" | \"medium\" | \"tight\" at the top level instead{escape}"
				)));
			}
		}
		let mut cfg: Self = merged
			.try_into()
			.map_err(|e: toml::de::Error| io::Error::Parse(e.to_string()))?;
		let preset = cfg.preset;
		preset.apply(&mut cfg);
		// serde's struct-level default pins data_dir to the *process* cwd. A
		// caller loading another root (hub merge, any cross-root tooling) must
		// get that root's store, never its own — re-pin when no config set it.
		if cfg.data_dir == Self::default().data_dir {
			cfg.data_dir = Self::default_in(cwd).data_dir;
		}
		cfg.data_dir = graviton_data_dir(&cfg.data_dir, cwd);
		Ok(cfg)
	}

	/// Where a detached child's captured output belongs: inside `data_dir`, so a
	/// relocated store keeps its logs in a directory kern owns. Taking the parent
	/// instead would drop `daemon.log` straight into `$HOME` for
	/// `data_dir = "/home/u/kernstore"`. Absolute by the time this runs, so a
	/// launch from a subdirectory logs where the graph lives.
	pub fn log_dir(&self) -> PathBuf {
		PathBuf::from(&self.data_dir).join("logs")
	}

	// `.git` may be a FILE (worktree/submodule): test existence, not `is_dir()`.
	pub fn resolve_root(start: &Path) -> PathBuf {
		for anc in start.ancestors() {
			if anc.join(".git").exists() {
				return anc.to_path_buf();
			}
		}
		for anc in start.ancestors() {
			if anc.join(".kern").is_dir() {
				return anc.to_path_buf();
			}
		}
		start.to_path_buf()
	}

	pub fn validate(&self) -> Result<(), String> {
		if self.embed.url.is_empty() {
			return Err("embed.url is required".into());
		}
		if self.embed.model.is_empty() {
			return Err("embed.model is required".into());
		}
		self.ingest.validate().map_err(|e| format!("ingest: {e}"))?;
		self.intake.validate().map_err(|e| format!("intake: {e}"))?;
		self
			.watcher
			.validate()
			.map_err(|e| format!("watcher: {e}"))?;
		let retrieval = self.retrieval.validate();
		if !retrieval.is_empty() {
			return Err(format!("retrieval: {}", retrieval.join("; ")));
		}
		Ok(())
	}

	pub fn reason_url(&self) -> &str {
		if self.reason.url.is_empty() {
			&self.embed.url
		} else {
			&self.reason.url
		}
	}

	pub fn reason_key(&self) -> &str {
		if self.reason.key.is_empty() {
			&self.embed.key
		} else {
			&self.reason.key
		}
	}

	/// One warning per configured LLM/embed URL whose host is not local to this
	/// machine. Empty when every configured URL is local (or empty). Pure — no
	/// I/O, no logging — so the caller (`boot_config`) owns the emit surface and
	/// the test owns the assertion. `reason.url` is checked raw, not via the
	/// `reason_url()` fallback, because an empty `reason.url` silently inherits
	/// `embed.url` and a warning for that would double-count the one provider.
	pub fn egress_warnings(&self) -> Vec<String> {
		let mut out = Vec::new();
		for (label, url) in [
			("embed.url", &self.embed.url),
			("reason.url", &self.reason.url),
		] {
			if !url.is_empty() && !crate::llm::is_local_url(url) {
				out.push(format!(
					"{label} ({url}) is non-local — all text sent to it egresses this machine"
				));
			}
		}
		out
	}

	/// Ollama-native knobs (`num_ctx`, `keep_alive`) a `/v1` (OpenAI-compat)
	/// endpoint silently ignores. One warning per knob a config sets on a `/v1`
	/// endpoint; default values are silent — a default `/v1` config is not
	/// "trying to tune" anything, so there is nothing to warn about. `reason.url`
	/// is checked raw for the same reason `egress_warnings` checks it raw: an
	/// empty `reason.url` inherits `embed.url`, and warning on the inherited
	/// value would double-count the one provider.
	pub fn native_knob_warnings(&self) -> Vec<String> {
		let mut out = Vec::new();
		if crate::llm::is_openai_compat(&self.embed.url) {
			if self.embed.num_ctx != 0 && self.embed.num_ctx != crate::llm::EMBED_NUM_CTX {
				out.push(format!(
					"embed.num_ctx = {} is ignored — embed.url ({}) is an OpenAI-compatible /v1 endpoint with no client-side context window",
					self.embed.num_ctx, self.embed.url
				));
			}
			if !self.embed.keep_alive.is_empty() && self.embed.keep_alive != crate::llm::EMBED_KEEP_ALIVE
			{
				out.push(format!(
					"embed.keep_alive = \"{}\" is ignored — embed.url ({}) is an OpenAI-compatible /v1 endpoint with no keep-alive option",
					self.embed.keep_alive, self.embed.url
				));
			}
		}
		if crate::llm::is_openai_compat(&self.reason.url) {
			if self.reason.num_ctx != 0 && self.reason.num_ctx != crate::llm::REASON_NUM_CTX {
				out.push(format!(
					"reason.num_ctx = {} is ignored — reason.url ({}) is an OpenAI-compatible /v1 endpoint with no client-side context window",
					self.reason.num_ctx, self.reason.url
				));
			}
			if !self.reason.keep_alive.is_empty()
				&& self.reason.keep_alive != crate::llm::REASON_KEEP_ALIVE
			{
				out.push(format!(
					"reason.keep_alive = \"{}\" is ignored — reason.url ({}) is an OpenAI-compatible /v1 endpoint with no keep-alive option",
					self.reason.keep_alive, self.reason.url
				));
			}
		}
		out
	}

	/// In WSL2 a Linux loopback URL (`127.0.0.1` / `localhost`) does not reach a
	/// Windows-host Ollama — the guest needs the RFC1918 gateway IP. Warn once
	/// per loopback endpoint when running under WSL, so the hand-pinning the
	/// LoCoMo run needed is no longer a silent failure. Non-WSL hosts are silent
	/// (loopback is correct there), and a non-loopback local URL (e.g. the WSL2
	/// gateway `172.27.x.x`) is already correct, so it is silent too.
	pub fn wsl_loopback_warnings(&self) -> Vec<String> {
		self.wsl_loopback_warnings_for(crate::llm::is_wsl())
	}

	/// Same as `wsl_loopback_warnings` with the WSL flag injected, so a test does
	/// not depend on `/proc/sys/kernel/osrelease` matching the runner.
	fn wsl_loopback_warnings_for(&self, in_wsl: bool) -> Vec<String> {
		if !in_wsl {
			return Vec::new();
		}
		let mut out = Vec::new();
		for (label, url) in [
			("embed.url", &self.embed.url),
			("reason.url", &self.reason.url),
		] {
			if !url.is_empty() && crate::llm::is_loopback_url(url) {
				out.push(format!(
					"{label} ({url}) is loopback, but kern is running under WSL — a Linux 127.0.0.1 does not reach a Windows-host Ollama. Pin the WSL2 gateway IP instead (e.g. the host side of /etc/resolv.conf, or `ip route show default`)"
				));
			}
		}
		out
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::PathBuf;

	#[test]
	fn load_gravitons_relative_data_dir_to_cwd() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		let kern = root.join(".kern");
		std::fs::create_dir_all(&kern).unwrap();
		std::fs::write(kern.join("kern.toml"), "data_dir = \".kern/data\"\n").unwrap();

		let cfg = Config::load(&root).expect("load");

		let got = PathBuf::from(&cfg.data_dir);
		assert!(got.is_absolute(), "data_dir must be absolute, got {got:?}");
		assert_eq!(got, root.join(".kern").join("data"));
	}

	#[test]
	fn load_of_a_foreign_root_pins_data_dir_to_that_root() {
		// Regression: with no config file, serde's default pinned data_dir to the
		// *process* cwd — a cross-root load (hub merge) then read its own store.
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".kern")).unwrap();

		let cfg = Config::load(&root).expect("load");
		assert_eq!(
			PathBuf::from(&cfg.data_dir),
			root.join(".kern").join("data"),
			"configless load must land in the passed root, not the process cwd"
		);
	}

	#[test]
	fn log_dir_stays_inside_the_data_dir_kern_owns() {
		let root = Path::new("/proj");
		assert_eq!(
			Config::default_in(root).log_dir(),
			root.join(".kern").join("data").join("logs")
		);

		let mut moved = Config::default_in(root);
		moved.data_dir = "/var/lib/kern/store".into();
		assert_eq!(
			moved.log_dir(),
			PathBuf::from("/var/lib/kern/store/logs"),
			"a relocated store keeps its logs inside itself — the parent may be $HOME"
		);
	}

	#[test]
	fn resolve_root_walks_up_to_nearest_kern_dir() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let deep = root.join("a").join("b");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn resolve_root_returns_start_when_no_kern_ancestor() {
		let dir = tempfile::tempdir().unwrap();
		let start = dir.path().canonicalize().unwrap();
		// Shield from stray .kern dirs in parent directories (e.g. /tmp/.kern from a running daemon)
		std::fs::create_dir_all(start.join(".kern")).unwrap();
		assert_eq!(Config::resolve_root(&start), start);
	}

	#[test]
	fn resolve_root_gravitons_at_git_root_when_no_kern() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".git")).unwrap();
		let deep = root.join("a").join("b");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn resolve_root_detects_git_as_a_file() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
		let deep = root.join("a");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn resolve_root_innermost_git_wins() {
		let dir = tempfile::tempdir().unwrap();
		let outer = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(outer.join(".git")).unwrap();
		let inner = outer.join("project");
		std::fs::create_dir_all(inner.join(".git")).unwrap();
		let deep = inner.join("src");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), inner);
	}

	#[test]
	fn resolve_root_prefers_git_root_over_deeper_kern() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".git")).unwrap();
		let sub = root.join("sub");
		std::fs::create_dir_all(sub.join(".kern")).unwrap();
		let deep = sub.join("deep");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn default_in_pins_data_dir_to_the_given_cwd_deterministically() {
		let cwd = Path::new("some_project_root");
		let cfg = Config::default_in(cwd);
		assert_eq!(
			cfg.data_dir,
			cwd.join(".kern").join("data").to_string_lossy()
		);
		assert_eq!(Config::default_in(cwd).data_dir, cfg.data_dir);
	}

	#[test]
	fn validate_requires_embed_and_surfaces_sub_config_invariants() {
		let cfg = Config::default_in(Path::new("x"));
		assert!(cfg.validate().is_ok(), "shipped defaults validate");

		let mut no_embed = Config::default_in(Path::new("x"));
		no_embed.embed.url = String::new();
		assert!(no_embed.validate().unwrap_err().contains("embed.url"));

		let mut bad_ingest = Config::default_in(Path::new("x"));
		bad_ingest.ingest.dedup_threshold = 2.0;
		let err = bad_ingest.validate().unwrap_err();
		assert!(
			err.contains("ingest"),
			"sub-config error is surfaced + tagged: {err}"
		);

		let mut bad_retr = Config::default_in(Path::new("x"));
		bad_retr.retrieval.seed_k = 0;
		let err = bad_retr.validate().unwrap_err();
		assert!(
			err.contains("retrieval"),
			"retrieval error surfaced + tagged: {err}"
		);
		assert!(err.contains("seed_k"), "the specific issue is named: {err}");
	}

	fn root_with(toml: &str) -> tempfile::TempDir {
		let dir = tempfile::tempdir().unwrap();
		let kern = dir.path().join(".kern");
		std::fs::create_dir_all(&kern).unwrap();
		std::fs::write(kern.join("kern.toml"), toml).unwrap();
		dir
	}

	#[test]
	fn configless_load_defaults_to_relaxed() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let cfg = Config::load(&root).expect("load");
		assert_eq!(cfg.preset, Preset::Relaxed);
		assert_eq!(cfg.retrieval.seed_k, 25);
		assert_eq!(cfg.heat.half_life_secs, 30 * 24 * 60 * 60);
	}

	#[test]
	fn preset_key_applies_its_tier() {
		let dir = root_with("preset = \"tight\"\n");
		let cfg = Config::load(&dir.path().canonicalize().unwrap()).expect("load");
		assert_eq!(cfg.preset, Preset::Tight);
		assert_eq!(cfg.retrieval.seed_k, 10);
		assert_eq!(cfg.heat.half_life_secs, 3 * 24 * 60 * 60);
	}

	#[test]
	fn preset_managed_sections_refuse_to_load() {
		for section in ["heat", "ingest", "retrieval"] {
			let dir = root_with(&format!("[{section}]\nanything = 1\n"));
			let err = Config::load(&dir.path().canonicalize().unwrap()).unwrap_err();
			let msg = err.to_string();
			assert!(
				msg.contains(section) && msg.contains("preset"),
				"[{section}] must be refused with a pointer to presets: {msg}"
			);
		}
	}

	// The same load-bearing point for the review lifecycle: a `review_policy` a
	// `kern.toml` cannot express is a policy nobody has, and the hold half of
	// ROADMAP item 21 is then unreachable no matter what the query surface
	// accepts. Both directions, because the exception must stay an exception.
	#[test]
	fn a_real_kern_toml_can_set_review_policy_and_nothing_else_in_ingest() {
		let dir = root_with("[ingest]\nreview_policy = { inline = \"pending\" }\n");
		let root = dir.path().canonicalize().unwrap();
		let cfg = Config::load_with_user(&root.join("no-such-user.toml"), &root)
			.expect("review_policy is not preset-managed");
		assert_eq!(
			cfg.ingest.review_policy.get("inline"),
			Some(&crate::base::types::ReviewState::Pending),
			"the policy a real file set has to reach the struct the ingest gate reads"
		);

		// The tuning key in the same table is still refused, so the preset stays
		// the only writer of what a preset owns.
		let dir =
			root_with("[ingest]\nreview_policy = { inline = \"pending\" }\ndedup_threshold = 0.5\n");
		let root = dir.path().canonicalize().unwrap();
		let err = Config::load_with_user(&root.join("no-such-user.toml"), &root).unwrap_err();
		assert!(
			err.to_string().contains("preset"),
			"a tuning knob smuggled in beside review_policy must still be refused: {err}"
		);
	}

	// The load-bearing half of per-source retention: a policy a `kern.toml`
	// cannot express is a policy nobody has. `[ingest]` accepts nothing but
	// `review_policy`, so the retention key lives in the two sections that
	// describe the sources themselves — and this proves a real file reaches the
	// struct.
	#[test]
	fn a_real_kern_toml_can_set_per_source_retention() {
		let dir = root_with(
			"[intake]\nretention_secs = 2592000\n\n[watcher]\nenabled = true\nretention_secs = 86400\n",
		);
		let root = dir.path().canonicalize().unwrap();
		let cfg = Config::load_with_user(&root.join("no-such-user.toml"), &root)
			.expect("a user-writable section must load");

		assert_eq!(
			cfg.intake.retention_secs, 2_592_000,
			"30 days on the intake"
		);
		assert_eq!(cfg.watcher.retention_secs, 86_400, "a day on the watcher");
		assert!(cfg.validate().is_ok(), "and the loaded config validates");
	}

	#[test]
	fn project_preset_beats_user_preset() {
		let dir = tempfile::tempdir().unwrap();
		let user = dir.path().join("user.toml");
		std::fs::write(&user, "preset = \"tight\"\n").unwrap();
		let root = root_with("preset = \"medium\"\n");
		let cfg = Config::load_with_user(&user, &root.path().canonicalize().unwrap()).expect("load");
		assert_eq!(cfg.preset, Preset::Medium);
		assert_eq!(cfg.retrieval.seed_k, 15);
	}

	#[test]
	fn unknown_preset_name_refuses_to_load() {
		let dir = root_with("preset = \"loose\"\n");
		let err = Config::load(&dir.path().canonicalize().unwrap()).unwrap_err();
		assert!(
			err.to_string().contains("relaxed"),
			"the error names the valid tiers: {err}"
		);
	}

	#[test]
	fn egress_warnings_flags_a_public_embed_url_and_silences_loopback() {
		let mut cfg = Config::default_in(Path::new("x"));
		// loopback embed url — no warning
		cfg.embed.url = "http://127.0.0.1:11434".into();
		assert!(cfg.egress_warnings().is_empty(), "loopback is local");

		// public embed url — one warning, naming embed.url
		cfg.embed.url = "https://api.openai.com".into();
		let w = cfg.egress_warnings();
		assert_eq!(w.len(), 1);
		assert!(w[0].contains("embed.url"), "names the field: {w:?}");
		assert!(w[0].contains("api.openai.com"), "names the host: {w:?}");
	}

	#[test]
	fn egress_warnings_reports_one_per_non_local_url() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "https://api.openai.com".into();
		cfg.reason.url = "http://203.0.113.5".into();
		let w = cfg.egress_warnings();
		assert_eq!(w.len(), 2, "one per non-local url: {w:?}");
		// empty reason.url inherits embed.url silently — must not double-count
		cfg.reason.url = String::new();
		assert_eq!(cfg.egress_warnings().len(), 1);
	}

	#[test]
	fn native_knob_warnings_silent_on_default_loopback() {
		let cfg = Config::default_in(Path::new("x"));
		// default is loopback Ollama, native, default knobs — nothing to warn
		assert!(
			cfg.native_knob_warnings().is_empty(),
			"{:?}",
			cfg.native_knob_warnings()
		);
	}

	#[test]
	fn native_knob_warnings_silent_on_a_v1_endpoint_with_default_knobs() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://localhost:8000/v1".into();
		// /v1 endpoint, but knobs still at default — not "trying to tune", silent
		assert!(
			cfg.native_knob_warnings().is_empty(),
			"{:?}",
			cfg.native_knob_warnings()
		);
	}

	#[test]
	fn native_knob_warnings_names_a_tuned_knob_on_a_v1_endpoint() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://localhost:8000/v1".into();
		cfg.embed.num_ctx = 8192; // non-default, ignored on /v1
		cfg.embed.keep_alive = "30m".into();
		let w = cfg.native_knob_warnings();
		assert_eq!(w.len(), 2, "one per tuned knob: {w:?}");
		assert!(w[0].contains("embed.num_ctx"), "names the knob: {w:?}");
		assert!(w[0].contains("8192"), "names the value: {w:?}");
		assert!(w[1].contains("embed.keep_alive"), "names the knob: {w:?}");
		assert!(w[1].contains("30m"), "names the value: {w:?}");
		// native (non-/v1) Ollama endpoint with the same tuned knobs — silent,
		// because there the knobs ARE sent
		cfg.embed.url = "http://localhost:11434".into();
		assert!(
			cfg.native_knob_warnings().is_empty(),
			"native endpoint honours the knobs"
		);
	}

	#[test]
	fn embed_config_default_carries_the_native_knob_constants() {
		let c = crate::config::embed::EmbedConfig::default();
		assert_eq!(c.num_ctx, crate::llm::EMBED_NUM_CTX);
		assert_eq!(c.keep_alive, crate::llm::EMBED_KEEP_ALIVE);
	}

	#[test]
	fn wsl_loopback_warnings_silent_off_wsl() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://127.0.0.1:11434".into();
		// not WSL -> loopback is correct, silent
		assert!(cfg.wsl_loopback_warnings_for(false).is_empty());
	}

	#[test]
	fn wsl_loopback_warnings_names_a_loopback_endpoint_under_wsl() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://127.0.0.1:11434".into();
		cfg.reason.url = "http://localhost:11434".into();
		let w = cfg.wsl_loopback_warnings_for(true);
		assert_eq!(w.len(), 2, "one per loopback endpoint: {w:?}");
		assert!(w[0].contains("embed.url"), "names the field: {w:?}");
		assert!(w[0].contains("127.0.0.1"), "names the host: {w:?}");
		assert!(w[0].contains("WSL"), "names the cause: {w:?}");
		// a non-loopback local URL (the WSL2 gateway) is already correct — silent
		cfg.embed.url = "http://172.27.176.1:11434".into();
		assert_eq!(
			cfg.wsl_loopback_warnings_for(true).len(),
			1,
			"gateway IP is not loopback"
		);
	}

	#[test]
	fn is_loopback_url_pins_loopback_only() {
		assert!(crate::llm::is_loopback_url("http://127.0.0.1:11434"));
		assert!(crate::llm::is_loopback_url("http://127.1.2.3:11434"));
		assert!(crate::llm::is_loopback_url("http://localhost:11434"));
		assert!(crate::llm::is_loopback_url("http://[::1]:8080"));
		// RFC1918 is local but NOT loopback — the WSL2 gateway falls here
		assert!(!crate::llm::is_loopback_url("http://172.27.176.1:11434"));
		assert!(!crate::llm::is_loopback_url("http://10.0.0.1:11434"));
		assert!(!crate::llm::is_loopback_url("https://api.openai.com"));
	}
}
