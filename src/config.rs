//! Configuration, whole: the resolved [`Config`] with every section
//! (embed, reason, gnn, graph, hub, ingest, intake, reload, retrieval, serve,
//! tick, watcher, gossip), the `.git`-first root resolution and deep merge of
//! global-then-project TOML in the io half, the tuning presets, secret
//! redirection, and the detached-log plumbing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::heat::HeatConfig;

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

	pub fn load(cwd: &Path) -> Result<Self, crate::config::Error> {
		let user = dirs::config_dir()
			.map(|d| d.join("kern").join("kern.toml"))
			.unwrap_or_else(|| cwd.join(".kern").join("kern.toml"));
		Self::load_with_user(&user, cwd)
	}

	/// `load` with the user scope named explicitly. A test that lets `load`
	/// resolve it reads the developer's real `~/.config/kern/kern.toml` and
	/// passes or fails on whatever happens to be on that machine.
	pub fn load_with_user(user: &Path, cwd: &Path) -> Result<Self, crate::config::Error> {
		let project = cwd.join(".kern").join("kern.toml");
		let merged = crate::config::merged_value(user, &project)?;
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
				return Err(crate::config::Error::Parse(format!(
					"[{section}] is preset-managed — set preset = \"relaxed\" | \"medium\" | \"tight\" at the top level instead{escape}"
				)));
			}
		}
		let mut cfg: Self = merged
			.try_into()
			.map_err(|e: toml::de::Error| crate::config::Error::Parse(e.to_string()))?;
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
			Some(&crate::base_types::ReviewState::Pending),
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
		let c = crate::config::EmbedConfig::default();
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

// ==== [io] ====

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
	#[error("io: {0}")]
	Io(String),
	#[error("parse: {0}")]
	Parse(String),
}

pub fn merged_value(user: &Path, project: &Path) -> Result<toml::Value, Error> {
	let user_v = read_value(user)?;
	let project_v = read_value(project)?;
	Ok(crate::config::seal_redirected(
		merge_deep(user_v, project_v.clone()),
		&project_v,
	))
}

fn read_value(path: &Path) -> Result<toml::Value, Error> {
	match std::fs::read_to_string(path) {
		// Parse as a document `toml::Table` — a bare-`Value` parse misreads a leading
		// `[section]` header as an array (see read_value_parses_leading_section_header).
		Ok(text) => text
			.parse::<toml::Table>()
			.map(toml::Value::Table)
			.map_err(|e| Error::Parse(e.to_string())),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			Ok(toml::Value::Table(toml::value::Table::new()))
		}
		Err(e) => Err(Error::Io(e.to_string())),
	}
}

/// Recursive merge at every depth: where both scopes hold a table the two are
/// merged key by key, so a project setting one field of a section never drops the
/// user's other fields in it. Arrays are leaves — `over` replaces, never appends
/// (`watcher.roots` and `gossip.peers` are complete lists, not accumulators).
fn merge_deep(base: toml::Value, over: toml::Value) -> toml::Value {
	match (base, over) {
		(toml::Value::Table(mut a), toml::Value::Table(b)) => {
			for (k, v) in b {
				let merged = match a.remove(&k) {
					Some(existing) => merge_deep(existing, v),
					None => v,
				};
				a.insert(k, merged);
			}
			toml::Value::Table(a)
		}
		(_, over) => over,
	}
}

#[cfg(test)]
mod io_tests {
	use super::*;

	#[test]
	fn read_value_parses_leading_section_header() {
		let dir = std::env::temp_dir().join(format!("cfgio_rv_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let p = dir.join("c.toml");
		std::fs::write(&p, "[section]\nenabled = true\n").unwrap();
		let v = read_value(&p).expect("read_value should parse a document");
		let enabled = v
			.get("section")
			.and_then(|s| s.get("enabled"))
			.and_then(|b| b.as_bool());
		assert_eq!(enabled, Some(true));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merged_value_merges_project_section_over_missing_user() {
		let dir = std::env::temp_dir().join(format!("cfgio_ll_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&project, "[section]\nenabled = true\n").unwrap();
		let merged = merged_value(&user, &project).expect("merged_value");
		let enabled = merged
			.get("section")
			.and_then(|s| s.get("enabled"))
			.and_then(|b| b.as_bool());
		assert_eq!(enabled, Some(true));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merged_value_project_field_wins_and_keeps_the_user_fields_it_omits() {
		let dir = std::env::temp_dir().join(format!("cfgio_ovr_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&user, "[embed]\nurl = \"user-url\"\nkey = \"secret\"\n").unwrap();
		std::fs::write(&project, "[embed]\nmodel = \"proj-model\"\n").unwrap();

		let merged = merged_value(&user, &project).expect("merged_value");
		let embed = merged
			.get("embed")
			.and_then(|v| v.as_table())
			.expect("embed table");
		assert_eq!(
			embed.get("model").and_then(|v| v.as_str()),
			Some("proj-model"),
			"the project leaf wins"
		);
		assert_eq!(
			embed.get("url").and_then(|v| v.as_str()),
			Some("user-url"),
			"a field the project omits is inherited, not lost"
		);
		assert_eq!(
			embed.get("key").and_then(|v| v.as_str()),
			Some("secret"),
			"the key rides along while the project leaves the endpoint alone"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merged_value_seals_the_key_when_the_project_redirects_the_endpoint() {
		let dir = std::env::temp_dir().join(format!("cfgio_seal_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&user, "[embed]\nurl = \"user-url\"\nkey = \"secret\"\n").unwrap();
		std::fs::write(&project, "[embed]\nurl = \"http://attacker.example/v1\"\n").unwrap();

		let merged = merged_value(&user, &project).expect("merged_value");
		let embed = merged
			.get("embed")
			.and_then(|v| v.as_table())
			.expect("embed table");
		assert_eq!(
			embed.get("url").and_then(|v| v.as_str()),
			Some("http://attacker.example/v1"),
			"the redirect itself still applies"
		);
		assert_eq!(
			embed.get("key").and_then(|v| v.as_str()),
			None,
			"a cloned repo redirecting the endpoint must not harvest the user's credential"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merge_deep_merges_nested_tables_at_depth() {
		let base: toml::Value = "[a.b]\nx = 1\ny = 2\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();
		let over: toml::Value = "[a.b]\ny = 9\n[a.c]\nz = 3\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();

		let merged = merge_deep(base, over);
		let b = merged.get("a").and_then(|a| a.get("b")).expect("a.b");
		assert_eq!(
			b.get("x").and_then(|v| v.as_integer()),
			Some(1),
			"depth-2 sibling survives"
		);
		assert_eq!(
			b.get("y").and_then(|v| v.as_integer()),
			Some(9),
			"depth-2 leaf overridden"
		);
		assert_eq!(
			merged
				.get("a")
				.and_then(|a| a.get("c"))
				.and_then(|c| c.get("z"))
				.and_then(|v| v.as_integer()),
			Some(3),
			"a table only `over` has is added"
		);
	}

	#[test]
	fn merge_deep_leaf_and_array_in_over_replace_the_base() {
		let base: toml::Value = "[w]\nenabled = true\nroots = [\"a\", \"b\"]\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();
		let over: toml::Value = "[w]\nroots = [\"c\"]\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();

		let merged = merge_deep(base, over);
		let w = merged.get("w").expect("w");
		assert_eq!(
			w.get("enabled").and_then(|v| v.as_bool()),
			Some(true),
			"the scalar `over` omits is kept"
		);
		let roots: Vec<&str> = w
			.get("roots")
			.and_then(|v| v.as_array())
			.expect("roots")
			.iter()
			.filter_map(|v| v.as_str())
			.collect();
		assert_eq!(
			roots,
			vec!["c"],
			"an array is a leaf: replaced, not concatenated"
		);
	}

	#[test]
	fn merge_deep_scalar_in_over_beats_a_table_in_base() {
		// The conflict must sit BELOW the top level: a top-level clash is settled by
		// the plain insert the pre-deep-merge code already did, so it proves nothing.
		let base: toml::Value = "[a.b]\nx = 1\n".parse::<toml::Table>().unwrap().into();
		let over: toml::Value = "[a]\nb = 7\n".parse::<toml::Table>().unwrap().into();

		let merged = merge_deep(base, over);
		assert_eq!(
			merged
				.get("a")
				.and_then(|a| a.get("b"))
				.and_then(|v| v.as_integer()),
			Some(7),
			"mismatched kinds one level down: `over` wins outright"
		);
	}

	#[test]
	fn merged_value_keeps_sections_present_in_only_one_scope() {
		let dir = std::env::temp_dir().join(format!("cfgio_keep_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&user, "[reason]\nmodel = \"qwen\"\n").unwrap();
		std::fs::write(&project, "[embed]\nurl = \"p\"\n").unwrap();

		let merged = merged_value(&user, &project).expect("merged_value");
		assert_eq!(
			merged
				.get("reason")
				.and_then(|s| s.get("model"))
				.and_then(|v| v.as_str()),
			Some("qwen"),
			"user-only [reason] survives",
		);
		assert!(
			merged.get("embed").is_some(),
			"project-only [embed] is present too"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}
}

// ==== [preset] ====

// The whole tuning surface: heat, dedup, and retrieval breadth belong to the
// preset, not to individual keys. `Config::load` refuses the [heat]/[ingest]/
// [retrieval] sections — `[ingest] review_policy` is the one key it lets
// through, and it is curation, not tuning — and `apply` runs AFTER the file is
// deserialized, so it is the only effective writer of these knobs regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
	#[default]
	Relaxed,
	Medium,
	Tight,
}

struct Tuning {
	half_life_secs: u64,
	dedup_threshold: f64,
	seed_k: usize,
	max_expansions: usize,
	max_deliver_results: usize,
}

impl Preset {
	/// Lowercase name matching the serde rendering, for health surfacing.
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Relaxed => "relaxed",
			Self::Medium => "medium",
			Self::Tight => "tight",
		}
	}

	fn tuning(&self) -> Tuning {
		match self {
			Self::Relaxed => Tuning {
				half_life_secs: 30 * 24 * 60 * 60,
				dedup_threshold: 0.98,
				seed_k: 25,
				max_expansions: 800,
				max_deliver_results: 40,
			},
			Self::Medium => Tuning {
				half_life_secs: 7 * 24 * 60 * 60,
				dedup_threshold: 0.95,
				seed_k: 15,
				max_expansions: 500,
				max_deliver_results: 25,
			},
			Self::Tight => Tuning {
				half_life_secs: 3 * 24 * 60 * 60,
				dedup_threshold: 0.90,
				seed_k: 10,
				max_expansions: 250,
				max_deliver_results: 12,
			},
		}
	}

	pub(crate) fn apply(&self, cfg: &mut Config) {
		let t = self.tuning();
		cfg.heat.half_life_secs = t.half_life_secs;
		cfg.ingest.dedup_threshold = t.dedup_threshold;
		cfg.retrieval.seed_k = t.seed_k;
		cfg.retrieval.max_expansions = t.max_expansions;
		cfg.retrieval.max_deliver_results = t.max_deliver_results;
	}
}

#[cfg(test)]
mod preset_tests {
	use super::*;
	use crate::heat::HeatConfig;
	use std::path::Path;

	fn applied(p: Preset) -> Config {
		let mut cfg = Config::default_in(Path::new("x"));
		p.apply(&mut cfg);
		cfg
	}

	#[test]
	fn every_preset_yields_a_valid_config() {
		for p in [Preset::Relaxed, Preset::Medium, Preset::Tight] {
			assert!(applied(p).validate().is_ok(), "{p:?} must validate");
		}
	}

	#[test]
	fn medium_matches_the_neutral_struct_defaults() {
		// The sub-config defaults are the medium anchor; this pins them together
		// so neither can drift without failing here.
		let t = Preset::Medium.tuning();
		let r = RetrievalConfig::default();
		assert_eq!(t.half_life_secs, HeatConfig::default().half_life_secs);
		assert_eq!(t.dedup_threshold, IngestConfig::default().dedup_threshold);
		assert_eq!(t.seed_k, r.seed_k);
		assert_eq!(t.max_expansions, r.max_expansions);
		assert_eq!(t.max_deliver_results, r.max_deliver_results);
	}

	#[test]
	fn relaxed_and_tight_move_the_knobs_in_opposite_directions() {
		let r = applied(Preset::Relaxed);
		let m = applied(Preset::Medium);
		let t = applied(Preset::Tight);
		assert!(r.heat.half_life_secs > m.heat.half_life_secs);
		assert!(t.heat.half_life_secs < m.heat.half_life_secs);
		assert!(r.retrieval.max_deliver_results > m.retrieval.max_deliver_results);
		assert!(t.retrieval.max_deliver_results < m.retrieval.max_deliver_results);
		assert!(r.ingest.dedup_threshold > m.ingest.dedup_threshold);
		assert!(t.ingest.dedup_threshold < m.ingest.dedup_threshold);
	}

	#[test]
	fn the_default_preset_is_relaxed() {
		assert_eq!(Preset::default(), Preset::Relaxed);
	}
}

// ==== [secrets] ====

const URL: &str = "url";
const KEY: &str = "key";

pub fn seal_redirected(mut merged: toml::Value, project: &toml::Value) -> toml::Value {
	let Some(proj) = project.as_table() else {
		return merged;
	};
	let redirected: Vec<String> = proj
		.iter()
		.filter(|(_, v)| v.get(URL).is_some() && v.get(KEY).is_none())
		.map(|(k, _)| k.clone())
		.collect();
	if let Some(out) = merged.as_table_mut() {
		for section in redirected {
			if let Some(toml::Value::Table(t)) = out.get_mut(&section) {
				t.remove(KEY);
			}
		}
	}
	merged
}

#[cfg(test)]
mod secrets_tests {
	use super::*;

	fn table(s: &str) -> toml::Value {
		toml::Value::Table(s.parse::<toml::Table>().expect("test toml parses"))
	}

	fn key_of(v: &toml::Value, section: &str) -> Option<String> {
		v.get(section)?.get(KEY)?.as_str().map(|s| s.to_string())
	}

	#[test]
	fn a_project_that_redirects_the_url_does_not_inherit_the_users_key() {
		let merged = table("[embed]\nurl = \"http://attacker.example/v1\"\nkey = \"sk-live\"\n");
		let project = table("[embed]\nurl = \"http://attacker.example/v1\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(
			key_of(&sealed, "embed"),
			None,
			"redirecting the endpoint must not carry the credential minted for another one"
		);
		assert!(
			sealed.get("embed").and_then(|e| e.get(URL)).is_some(),
			"only the key is sealed; the redirect itself still applies"
		);
	}

	#[test]
	fn a_project_that_leaves_the_url_alone_keeps_inheriting_the_key() {
		let merged =
			table("[embed]\nurl = \"https://api.openai.com/v1\"\nkey = \"sk-live\"\nmodel = \"m\"\n");
		let project = table("[embed]\nmodel = \"m\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(
			key_of(&sealed, "embed").as_deref(),
			Some("sk-live"),
			"the whole point of layering: a user-level key survives a project-level model"
		);
	}

	#[test]
	fn a_project_supplying_its_own_key_with_its_own_url_keeps_it() {
		let merged = table("[embed]\nurl = \"http://local/v1\"\nkey = \"project-key\"\n");
		let project = table("[embed]\nurl = \"http://local/v1\"\nkey = \"project-key\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(key_of(&sealed, "embed").as_deref(), Some("project-key"));
	}

	#[test]
	fn sealing_is_per_section_not_global() {
		let merged = table(
			"[embed]\nurl = \"http://attacker/v1\"\nkey = \"sk-live\"\n\
			 [reason]\nurl = \"https://api.openai.com/v1\"\nkey = \"sk-live\"\n",
		);
		let project = table("[embed]\nurl = \"http://attacker/v1\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(
			key_of(&sealed, "embed"),
			None,
			"the redirected section is sealed"
		);
		assert_eq!(
			key_of(&sealed, "reason").as_deref(),
			Some("sk-live"),
			"a section the project never touched is untouched"
		);
	}
}

// ==== [embed] ====

pub const DEFAULT_EMBED_URL: &str = "http://localhost:11434";
// Dimension-locked into the graph on first ingest: changing the model later
// requires `kern reembed` or stored vectors mismatch and search silently misses.
pub const DEFAULT_EMBED_MODEL: &str = "qwen3-embedding:0.6b";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedConfig {
	pub url: String,
	pub model: String,
	pub key: String,
	// Ollama-native only; ignored on `/v1` (warned at boot). 0 keeps the default.
	pub num_ctx: u64,
	// Ollama-native only; ignored on `/v1` (warned at boot). Empty keeps the default.
	pub keep_alive: String,
}

impl Default for EmbedConfig {
	fn default() -> Self {
		Self {
			url: DEFAULT_EMBED_URL.into(),
			model: DEFAULT_EMBED_MODEL.into(),
			key: String::new(),
			num_ctx: crate::llm::EMBED_NUM_CTX,
			keep_alive: crate::llm::EMBED_KEEP_ALIVE.into(),
		}
	}
}

#[cfg(test)]
mod embed_tests {
	use super::*;

	#[test]
	fn default_uses_the_shared_constants() {
		let c = EmbedConfig::default();
		assert_eq!(c.url, DEFAULT_EMBED_URL);
		assert_eq!(c.model, DEFAULT_EMBED_MODEL);
		assert!(c.key.is_empty(), "no API key by default (local Ollama)");
	}
}

// ==== [reason] ====

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasonConfig {
	pub url: String,
	pub model: String,
	pub key: String,
	// Ceiling for one `complete` — the distill leg's slowest call. It was a
	// `const` nobody chose for this leg; the default is the number it was, so an
	// unconfigured kern posts under exactly the same bound. 0 keeps the default.
	pub timeout_secs: u64,
	// Ollama-native only; ignored on `/v1` (warned at boot). 0 keeps the default.
	pub num_ctx: u64,
	// Ollama-native only; ignored on `/v1` (warned at boot). Empty keeps the default.
	pub keep_alive: String,
}

const DEFAULT_REASON_URL: &str = "http://localhost:11434";

pub const DEFAULT_REASON_MODEL: &str = "granite4:3b";

// Slow CPU inference / large RAG prompts / long streams run past anything less.
pub const DEFAULT_REASON_TIMEOUT_SECS: u64 = 600;

impl Default for ReasonConfig {
	fn default() -> Self {
		Self {
			url: DEFAULT_REASON_URL.into(),
			model: DEFAULT_REASON_MODEL.into(),
			key: String::new(),
			timeout_secs: DEFAULT_REASON_TIMEOUT_SECS,
			num_ctx: crate::llm::REASON_NUM_CTX,
			keep_alive: crate::llm::REASON_KEEP_ALIVE.into(),
		}
	}
}

// ==== [gnn] ====

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct GnnConfig {
	pub self_weight: f64,
	pub min_weight: f64,
	pub min_thoughts: usize,
	pub train_epochs: usize,
	pub train_learning_rate: f64,
}

impl Default for GnnConfig {
	fn default() -> Self {
		Self {
			self_weight: crate::gnn::propagate::DEFAULT_SELF_WEIGHT,
			min_weight: crate::gnn::propagate::DEFAULT_MIN_WEIGHT,
			min_thoughts: crate::gnn::propagate::DEFAULT_MIN_THOUGHTS,
			train_epochs: crate::gnn::propagate::DEFAULT_TRAIN_EPOCHS,
			train_learning_rate: crate::gnn::propagate::DEFAULT_TRAIN_LEARNING_RATE,
		}
	}
}

impl From<GnnConfig> for crate::gnn::propagate::GnnConfig {
	fn from(c: GnnConfig) -> Self {
		crate::gnn::propagate::GnnConfig {
			self_weight: c.self_weight,
			min_weight: c.min_weight,
			min_thoughts: c.min_thoughts,
			train_epochs: c.train_epochs,
			train_learning_rate: c.train_learning_rate,
		}
	}
}

#[cfg(test)]
mod reason_tests {
	use super::*;

	#[test]
	fn from_maps_every_field_without_drift() {
		let serde_cfg = GnnConfig {
			self_weight: 0.11,
			min_weight: 0.22,
			min_thoughts: 33,
			train_epochs: 44,
			train_learning_rate: 0.55,
		};
		let runtime: crate::gnn::propagate::GnnConfig = serde_cfg.into();
		assert_eq!(runtime.self_weight, 0.11);
		assert_eq!(runtime.min_weight, 0.22);
		assert_eq!(runtime.min_thoughts, 33);
		assert_eq!(runtime.train_epochs, 44);
		assert_eq!(runtime.train_learning_rate, 0.55);
	}

	#[test]
	fn serde_default_equals_the_runtime_default() {
		let runtime: crate::gnn::propagate::GnnConfig = GnnConfig::default().into();
		let rd = crate::gnn::propagate::GnnConfig::defaults();
		assert_eq!(runtime.self_weight, rd.self_weight);
		assert_eq!(runtime.min_weight, rd.min_weight);
		assert_eq!(runtime.min_thoughts, rd.min_thoughts);
		assert_eq!(runtime.train_epochs, rd.train_epochs);
		assert_eq!(runtime.train_learning_rate, rd.train_learning_rate);
	}
}

// ==== [graph] ====

use crate::base_constants::KERN_CAP_DISABLED;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
	pub max_kerns: usize,
	pub max_ledger_entries: usize,
	pub disk_threshold: usize,
}

impl Default for GraphConfig {
	fn default() -> Self {
		Self {
			// A conservative resident bound (ROADMAP item 83): most projects carry
			// <10 kerns. Eviction is proven safe — get_mut auto-loads, so the
			// post-register children-push lands on a reloaded copy that persists
			// (spawn_unnamed_child_under_cap_keeps_the_child_in_parent_children);
			// the old "drops unpersisted children pushes" comment was stale. 128
			// bounds the pathological case; eviction unloads to the cold tier, it
			// never forgets. disk_threshold stays disabled until item 75 (DiskANN
			// crash consistency) closes — arming it exposes the spill crash window.
			max_kerns: 128,
			max_ledger_entries: 10_000,
			disk_threshold: KERN_CAP_DISABLED,
		}
	}
}

#[cfg(test)]
mod gnn_tests {
	use super::*;

	#[test]
	fn default_bounds_resident_kerns_conservatively() {
		// 128 is a safety bound, not a tuning knob: normal use is <10 kerns, and
		// eviction is proven safe (see GraphConfig::default). `usize::MAX` stays
		// the uncapped marker for an explicit opt-out.
		assert_eq!(GraphConfig::default().max_kerns, 128);
		assert_eq!(
			KERN_CAP_DISABLED,
			usize::MAX,
			"sentinel value is the uncapped marker"
		);
	}

	#[test]
	fn default_disables_disk_spill() {
		assert_eq!(GraphConfig::default().disk_threshold, KERN_CAP_DISABLED);
	}
}

// ==== [hub] ====

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
	// `kern mcp` spawns a detached machine-level hub when none answers, same as
	// it already auto-spawns a project daemon. false = hub is opt-in via
	// `kern hub`; the direct-connect fallback works either way.
	pub auto_start: bool,
	// A client attaching to a daemon built from a different binary, or booted
	// against a different config, restarts it before proxying. Without this a
	// long-lived daemon serves stale code and stale config indefinitely — the
	// failure that makes every shipped fix look like it did nothing.
	pub auto_restart: bool,
}

impl Default for HubConfig {
	fn default() -> Self {
		Self {
			auto_start: true,
			auto_restart: true,
		}
	}
}

// ==== [ingest] ====

use crate::base_constants::INGEST_DEDUP_THRESHOLD;
use crate::base_types::{EntityKind, Source};
use crate::ingest::ReviewPolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestConfig {
	pub dedup_threshold: f64,
	/// Per-kind overrides indexed by `EntityKind as u8` (Fact=0 .. Conclusion=4).
	/// `None` falls back to `dedup_threshold`; default `[None; 5]` is
	/// bit-identical today. An operator can ask Facts to dedup tighter than
	/// Claims without tightening both (ROADMAP item 48 beside).
	#[serde(default = "default_dedup_threshold_by_kind")]
	pub dedup_threshold_by_kind: [Option<f64>; EntityKind::Conclusion as usize + 1],
	// Per-source-scheme curation policy, keyed on `Source::scheme()` — file,
	// ticket, session, agent, inline. An absent key is `active`, so the empty
	// default leaves every ingest retrievable exactly as before; `file =
	// "pending"` is how a host holds its watcher's auto-distilled claims out of
	// an `exclude_pending` query until `promote` curates them. Like
	// `source_trust` this weights the CHANNEL, not the author (ROADMAP 20).
	pub review_policy: ReviewPolicy,
}

fn default_dedup_threshold_by_kind() -> [Option<f64>; EntityKind::Conclusion as usize + 1] {
	[None; EntityKind::Conclusion as usize + 1]
}

impl Default for IngestConfig {
	fn default() -> Self {
		Self {
			dedup_threshold: INGEST_DEDUP_THRESHOLD,
			dedup_threshold_by_kind: default_dedup_threshold_by_kind(),
			review_policy: ReviewPolicy::new(),
		}
	}
}

impl IngestConfig {
	pub fn validate(&self) -> Result<(), String> {
		// A misspelled scheme would hold nothing back and read as a working knob,
		// so an unknown key is an error rather than a silent no-op.
		for scheme in self.review_policy.keys() {
			if Source::parse_scheme(scheme).is_none() {
				return Err(format!(
					"review_policy key {scheme:?} is not a source scheme (file, ticket, session, agent, inline)"
				));
			}
		}
		crate::ingest::Config {
			dedup_threshold: self.dedup_threshold,
			dedup_threshold_by_kind: self.dedup_threshold_by_kind,
			..Default::default()
		}
		.validate()
	}
}

#[cfg(test)]
mod graph_tests {
	use super::*;
	use crate::base_types::ReviewState;

	#[test]
	fn default_validates_and_bad_knobs_are_rejected() {
		assert!(
			IngestConfig::default().validate().is_ok(),
			"shipped defaults are valid"
		);

		let out_of_range = IngestConfig {
			dedup_threshold: 2.0,
			..Default::default()
		};
		assert!(
			out_of_range.validate().is_err(),
			"threshold outside [0,1] is rejected"
		);
	}

	#[test]
	fn an_unknown_review_policy_scheme_is_flagged() {
		let typo = IngestConfig {
			review_policy: ReviewPolicy::from([("files".to_string(), ReviewState::Pending)]),
			..Default::default()
		};
		assert!(
			typo.validate().unwrap_err().contains("review_policy"),
			"a scheme that names nothing must not read as a working policy"
		);

		let good = IngestConfig {
			review_policy: ReviewPolicy::from([("file".to_string(), ReviewState::Pending)]),
			..Default::default()
		};
		assert!(good.validate().is_ok(), "a real scheme is accepted");
	}
}

// ==== [intake] ====

// `dir` MUST stay cwd-relative and independent of `data_dir`:
// the MCP server resolves it from session cwd; deriving from data_dir breaks that contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IntakeConfig {
	pub enabled: bool,
	pub dir: String,
	pub poll_secs: u64,
	pub done_retention_secs: u64,
	// The TTL stamped on everything this queue ingests — "everything from this
	// source expires in 30 days", said once instead of on every call. Distinct
	// from `done_retention_secs`, which prunes the archived *files*: this one is
	// `valid_until` on the entity. 0 = no TTL, matching `--retention-secs`.
	// It lives here rather than in `[ingest]` because `Config::load_with_user`
	// refuses every tuning key in a user-written `[ingest]` — that table is
	// preset-owned, and its one exception is `review_policy`, which is curation
	// rather than tuning. A key no `kern.toml` can set is a key that ships dead.
	pub retention_secs: u64,
}

impl Default for IntakeConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			dir: ".kern/intake".into(),
			poll_secs: 5,
			done_retention_secs: 7 * 24 * 60 * 60,
			retention_secs: 0,
		}
	}
}

impl IntakeConfig {
	pub fn validate(&self) -> Result<(), String> {
		if self.enabled && self.poll_secs == 0 {
			return Err("poll_secs must be > 0 (0 busy-loops the intake drain)".into());
		}
		// Refuse a retention that can never become a deadline at boot, rather
		// than logging it once per drain pass for the life of the daemon.
		crate::ingest::valid_until_from_retention(self.retention_secs)?;
		Ok(())
	}
}

#[cfg(test)]
mod hub_tests {
	use super::*;

	#[test]
	fn defaults_are_on_with_sane_tunables() {
		let c = IntakeConfig::default();
		assert!(c.enabled);
		assert_eq!(c.dir, ".kern/intake");
		assert_eq!(c.poll_secs, 5);
		assert_eq!(c.done_retention_secs, 604_800, "7 days in seconds");
		assert_eq!(c.retention_secs, 0, "no standing TTL unless a host asks");
	}

	#[test]
	fn validate_rejects_a_retention_that_can_never_become_a_deadline() {
		let unusable = IntakeConfig {
			retention_secs: u64::MAX,
			..Default::default()
		};
		assert!(
			unusable.validate().unwrap_err().contains("overflows"),
			"a retention no clock can represent is refused at load, not per drain"
		);

		let thirty_days = IntakeConfig {
			retention_secs: 30 * 24 * 60 * 60,
			..Default::default()
		};
		assert!(thirty_days.validate().is_ok(), "a real policy is accepted");
	}

	#[test]
	fn validate_rejects_zero_poll_only_when_enabled() {
		assert!(
			IntakeConfig::default().validate().is_ok(),
			"default (enabled, non-zero poll) is valid"
		);

		let zero_poll = IntakeConfig {
			enabled: true,
			poll_secs: 0,
			..Default::default()
		};
		assert!(zero_poll.validate().unwrap_err().contains("poll_secs"));

		let disabled_zero = IntakeConfig {
			enabled: false,
			poll_secs: 0,
			..Default::default()
		};
		assert!(
			disabled_zero.validate().is_ok(),
			"disabled intake ignores its poll interval"
		);
	}
}

// ==== [reload] ====

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ReloadConfig {
	// The daemon watches its own binary and hands the socket to a freshly
	// spawned successor when the file changes. Unix only; on Windows the
	// client-side auto-restart covers staleness instead.
	pub enabled: bool,
	pub poll_secs: u64,
}

impl Default for ReloadConfig {
	fn default() -> Self {
		Self {
			enabled: true,
			poll_secs: 3,
		}
	}
}

// ==== [retrieval] ====

use std::collections::BTreeMap;

use crate::base_constants as constants;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeWeights {
	pub content: f64,
	pub reason: f64,
	pub edge: f64,
}

impl Default for ModeWeights {
	fn default() -> Self {
		Self {
			content: constants::DEFAULT_WEIGHT_CONTENT,
			reason: constants::DEFAULT_WEIGHT_REASON,
			edge: constants::DEFAULT_WEIGHT_EDGE,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetrievalConfig {
	pub seed_k: usize,
	pub max_expansions: usize,
	pub decay: f64,
	pub qbst_access_weight: f64,
	pub qbst_recency_weight: f64,
	pub qbst_recency_half_life_secs: u64,
	pub qbst_cap: f64,
	pub refine_traversal_weight: f64,
	pub refine_boost_cap: f64,
	// Ceiling on the summed source-weighted edge credit an entity can earn from
	// the walk. 0.0 turns traversal credit off; the cap is what keeps a
	// well-connected node from outranking a direct match on edge volume.
	pub traversal_credit_cap: f64,
	// Multiplier on each edge's credit contribution before the cap.
	pub traversal_credit_weight: f64,
	pub fact_score_boost: f64,
	pub gravity_weight: f64,
	// Multiplier on the final score of an entity held in a `remote-*` phantom kern.
	// Federation is unauthenticated: this is what stops peer-supplied content from
	// outranking local knowledge. 1.0 disables the penalty; 0.0 keeps remote
	// entities retrievable but always last.
	pub remote_trust_weight: f64,
	// Per-source-scheme trust prior, keyed on `Source::scheme()` — file, ticket,
	// session, agent, inline. An absent key is 1.0, so the empty default leaves
	// every score bit-identical. This weights the CHANNEL a claim arrived on, not
	// its author: `kern ingest` and an MCP agent's default ingest both write
	// `inline`, so no key here separates a human from an agent (ROADMAP 20).
	pub source_trust: BTreeMap<String, f64>,
	pub min_deliver_score: f64,
	pub max_deliver_results: usize,
	pub important_min_cosine: f64,
	pub important_access_threshold: i32,
	pub weights_content: ModeWeights,
	pub weights_reason: ModeWeights,
	pub weights_hybrid: ModeWeights,
	pub rrf_k: f64,
	pub rrf_global_weight: f64,
	pub dedup_by_section: bool,
	pub mmr_enabled: bool,
	pub mmr_lambda: f64,
	pub mmr_pool_size: usize,
	pub lexical_enabled: bool,
	pub bm25_k1: f64,
	pub bm25_b: f64,
	// Late-fusion BM25 bonus added to a delivered result's score after the
	// content/edge boosts and before MMR, so an entity whose text shares exact
	// query terms floats to the top of the list. 0.0 disables it and leaves
	// every score bit-identical to the pre-knob baseline. Scaled by the query's
	// max BM25 score so the bonus is magnitude-comparable across corpora.
	pub lexical_top_boost: f64,
	pub pagerank_enabled: bool,
	pub pagerank_damping: f64,
	pub pagerank_iters: usize,
	pub pagerank_top_k: usize,
}

impl Default for RetrievalConfig {
	fn default() -> Self {
		Self {
			seed_k: 15,
			max_expansions: 500,
			decay: 0.25,
			qbst_access_weight: constants::QBST_ACCESS_WEIGHT,
			qbst_recency_weight: constants::QBST_RECENCY_WEIGHT,
			qbst_recency_half_life_secs: constants::QBST_RECENCY_HALF_LIFE.as_secs(),
			qbst_cap: constants::QBST_CAP,
			refine_traversal_weight: constants::REFINE_TRAVERSAL_WEIGHT,
			refine_boost_cap: constants::REFINE_BOOST_CAP,
			traversal_credit_cap: constants::TRAVERSAL_CREDIT_CAP,
			traversal_credit_weight: constants::TRAVERSAL_CREDIT_WEIGHT,
			fact_score_boost: constants::FACT_SCORE_BOOST,
			gravity_weight: 0.15,
			remote_trust_weight: 0.4,
			source_trust: BTreeMap::new(),
			min_deliver_score: 0.0,
			max_deliver_results: 25,
			important_min_cosine: constants::IMPORTANT_MIN_COSINE,
			important_access_threshold: constants::IMPORTANT_ACCESS_THRESHOLD,
			weights_content: ModeWeights {
				content: 0.70,
				reason: 0.15,
				edge: 0.15,
			},
			weights_reason: ModeWeights {
				content: 0.20,
				reason: 0.60,
				edge: 0.20,
			},
			weights_hybrid: ModeWeights::default(),
			rrf_k: 60.0,
			rrf_global_weight: 0.5,
			dedup_by_section: true,
			mmr_enabled: true,
			mmr_lambda: 0.75,
			mmr_pool_size: 50,
			lexical_enabled: true,
			bm25_k1: 1.2,
			bm25_b: 0.75,
			lexical_top_boost: 0.0,
			pagerank_enabled: true,
			pagerank_damping: 0.85,
			pagerank_iters: 25,
			pagerank_top_k: 100,
		}
	}
}

impl RetrievalConfig {
	pub fn validate(&self) -> Vec<String> {
		let mut errs = Vec::new();

		for (name, w) in [
			("content", &self.weights_content),
			("reason", &self.weights_reason),
			("hybrid", &self.weights_hybrid),
		] {
			let sum = w.content + w.reason + w.edge;
			if (sum - 1.0).abs() > 0.01 {
				errs.push(format!("weights_{name} sum to {sum:.3}, expected ~1.0"));
			}
		}

		for (name, v) in [
			("mmr_lambda", self.mmr_lambda),
			("bm25_b", self.bm25_b),
			("remote_trust_weight", self.remote_trust_weight),
		] {
			if !(0.0..=1.0).contains(&v) {
				errs.push(format!("{name} ({v}) must be in [0.0, 1.0]"));
			}
		}

		// A misspelled scheme would weight nothing at all and read as a working
		// knob, so an unknown key is an error rather than a silent no-op.
		for (scheme, w) in &self.source_trust {
			if Source::parse_scheme(scheme).is_none() {
				errs.push(format!(
					"source_trust key {scheme:?} is not a source scheme (file, ticket, session, agent, inline)"
				));
			}
			if !w.is_finite() || *w < 0.0 {
				errs.push(format!("source_trust[{scheme:?}] ({w}) must be >= 0.0"));
			}
		}

		if self.bm25_k1 < 0.0 {
			errs.push(format!("bm25_k1 ({}) must be >= 0.0", self.bm25_k1));
		}
		if !self.lexical_top_boost.is_finite() || self.lexical_top_boost < 0.0 {
			errs.push(format!(
				"lexical_top_boost ({}) must be finite and >= 0.0",
				self.lexical_top_boost
			));
		}

		if !(0.0..1.0).contains(&self.pagerank_damping) {
			errs.push(format!(
				"pagerank_damping ({}) must be in [0.0, 1.0)",
				self.pagerank_damping
			));
		}

		if self.traversal_credit_cap < 0.0 {
			errs.push(format!(
				"traversal_credit_cap ({}) must be >= 0.0",
				self.traversal_credit_cap
			));
		}
		if self.traversal_credit_weight < 0.0 {
			errs.push(format!(
				"traversal_credit_weight ({}) must be >= 0.0",
				self.traversal_credit_weight
			));
		}
		if self.gravity_weight < 0.0 {
			errs.push(format!(
				"gravity_weight ({}) must be >= 0.0",
				self.gravity_weight
			));
		}
		if self.rrf_k < 0.0 {
			errs.push(format!("rrf_k ({}) must be >= 0.0", self.rrf_k));
		}
		if self.seed_k == 0 {
			errs.push("seed_k must be >= 1 (0 seeds nothing, so every query is empty)".to_string());
		}
		if self.max_deliver_results == 0 {
			errs.push("max_deliver_results must be >= 1 (0 delivers nothing)".to_string());
		}

		errs
	}
}

#[cfg(test)]
mod ingest_tests {
	use super::*;

	#[test]
	fn default_config_is_valid() {
		assert!(
			RetrievalConfig::default().validate().is_empty(),
			"shipped defaults must validate"
		);
	}

	#[test]
	fn weights_not_summing_to_one_are_flagged() {
		let mut cfg = RetrievalConfig::default();
		cfg.weights_hybrid.content = 0.9;
		let errs = cfg.validate();
		assert!(
			errs.iter().any(|e| e.contains("weights_hybrid")),
			"got {errs:?}"
		);
	}

	#[test]
	fn out_of_range_bm25_params_are_flagged() {
		let bad_b = RetrievalConfig {
			bm25_b: 2.0,
			..Default::default()
		};
		assert!(
			bad_b.validate().iter().any(|e| e.contains("bm25_b")),
			"bm25_b > 1"
		);

		let neg_k1 = RetrievalConfig {
			bm25_k1: -0.5,
			..Default::default()
		};
		assert!(
			neg_k1.validate().iter().any(|e| e.contains("bm25_k1")),
			"negative bm25_k1"
		);
	}

	// A typo'd scheme weights nothing and reads exactly like a working knob, which
	// is the failure an operator cannot see from the ranking.
	#[test]
	fn an_unknown_or_negative_source_trust_key_is_flagged() {
		let typo = RetrievalConfig {
			source_trust: BTreeMap::from([("files".to_string(), 0.5)]),
			..Default::default()
		};
		assert!(
			typo.validate().iter().any(|e| e.contains("files")),
			"got {:?}",
			typo.validate()
		);

		let negative = RetrievalConfig {
			source_trust: BTreeMap::from([("file".to_string(), -0.5)]),
			..Default::default()
		};
		assert!(
			negative.validate().iter().any(|e| e.contains("file")),
			"got {:?}",
			negative.validate()
		);

		let ok = RetrievalConfig {
			source_trust: BTreeMap::from([("agent".to_string(), 1.5)]),
			..Default::default()
		};
		assert!(
			ok.validate().is_empty(),
			"a real scheme above 1.0 lifts it over baseline and is valid: {:?}",
			ok.validate()
		);
	}

	#[test]
	fn retrieval_breaking_values_are_flagged() {
		let neg_rrf = RetrievalConfig {
			rrf_k: -1.0,
			..Default::default()
		};
		assert!(
			neg_rrf.validate().iter().any(|e| e.contains("rrf_k")),
			"negative rrf_k"
		);

		let zero_seed = RetrievalConfig {
			seed_k: 0,
			..Default::default()
		};
		assert!(
			zero_seed.validate().iter().any(|e| e.contains("seed_k")),
			"seed_k 0"
		);

		let zero_deliver = RetrievalConfig {
			max_deliver_results: 0,
			..Default::default()
		};
		assert!(
			zero_deliver
				.validate()
				.iter()
				.any(|e| e.contains("max_deliver_results")),
			"max_deliver_results 0"
		);

		let neg_gravity = RetrievalConfig {
			gravity_weight: -0.1,
			..Default::default()
		};
		assert!(
			neg_gravity
				.validate()
				.iter()
				.any(|e| e.contains("gravity_weight")),
			"negative gravity_weight"
		);

		let zero_rrf = RetrievalConfig {
			rrf_k: 0.0,
			..Default::default()
		};
		assert!(
			!zero_rrf.validate().iter().any(|e| e.contains("rrf_k")),
			"rrf_k 0 is valid, must not flag"
		);

		let neg_boost = RetrievalConfig {
			lexical_top_boost: -0.1,
			..Default::default()
		};
		assert!(
			neg_boost
				.validate()
				.iter()
				.any(|e| e.contains("lexical_top_boost")),
			"negative lexical_top_boost"
		);
	}
}

// ==== [serve] ====

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
mod intake_tests {
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

// ==== [tick] ====

use crate::base_constants::{
	KERN_IDLE_TIMEOUT, TICK_INTERVAL_SECS, TICK_MAX_CLUSTER_SAMPLE, TICK_QUEUE_CAPACITY,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct TickConfig {
	pub max_cluster_sample: usize,
	pub queue_capacity: usize,
	// `0` disables the driver: an idle daemon never decays heat or evicts cold nodes.
	pub interval_secs: u64,
	// `0` keeps every loaded kern resident forever.
	pub kern_idle_timeout_secs: u64,
}

impl Default for TickConfig {
	fn default() -> Self {
		Self {
			max_cluster_sample: TICK_MAX_CLUSTER_SAMPLE,
			queue_capacity: TICK_QUEUE_CAPACITY,
			interval_secs: TICK_INTERVAL_SECS,
			kern_idle_timeout_secs: KERN_IDLE_TIMEOUT.as_secs(),
		}
	}
}

// ==== [watcher] ====

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WatcherConfig {
	pub enabled: bool,
	pub roots: Vec<String>,
	// The TTL stamped on every document this watcher sinks — the watched roots
	// are one source, so their retention is one policy. Same reason as
	// `intake.retention_secs` for living here and not in the preset-owned
	// `[ingest]`: a user's `kern.toml` may set nothing in that table but
	// `review_policy`, and a tuning key there refuses to load.
	// 0 = no TTL. Derived `Default` gives 0, which is the shipped behaviour.
	pub retention_secs: u64,
}

impl WatcherConfig {
	pub fn validate(&self) -> Result<(), String> {
		crate::ingest::valid_until_from_retention(self.retention_secs)?;
		Ok(())
	}

	pub fn effective_roots(&self, cwd: &Path) -> Vec<PathBuf> {
		if !self.enabled {
			return Vec::new();
		}
		if self.roots.is_empty() {
			vec![cwd.to_path_buf()]
		} else {
			// Pinned to `cwd`, not handed to `notify` as written: a relative root
			// makes every event path relative too, and the daemon's off-limits
			// prefixes (`data_dir`, `intake.dir`) are absolute. Two coordinate
			// systems is how the watcher ends up re-ingesting kern's own state.
			self
				.roots
				.iter()
				.map(|r| {
					let p = Path::new(r);
					if p.is_absolute() {
						p.to_path_buf()
					} else {
						cwd.join(p)
					}
				})
				.collect()
		}
	}
}

#[cfg(test)]
mod reload_tests {
	use super::*;

	#[test]
	fn effective_roots_falls_back_to_cwd_when_enabled_and_empty() {
		let cfg = WatcherConfig {
			enabled: true,
			..Default::default()
		};
		assert_eq!(
			cfg.effective_roots(Path::new("/proj")),
			vec![PathBuf::from("/proj")]
		);
	}

	#[test]
	fn effective_roots_uses_configured_roots_when_present() {
		let cfg = WatcherConfig {
			enabled: true,
			roots: vec!["a".into(), "/elsewhere/b".into()],
			..Default::default()
		};
		assert_eq!(
			cfg.effective_roots(Path::new("/proj")),
			vec![PathBuf::from("/proj/a"), PathBuf::from("/elsewhere/b")],
			"configured roots win over the cwd fallback, and a relative one is \
			 pinned to cwd so event paths and the denied prefixes share a frame"
		);
	}

	#[test]
	fn effective_roots_is_empty_when_disabled() {
		let cfg = WatcherConfig {
			enabled: false,
			roots: vec!["a".into()],
			..Default::default()
		};
		assert!(
			cfg.effective_roots(Path::new("/proj")).is_empty(),
			"a disabled watcher has nothing to watch even with roots set"
		);
	}
}

// ==== [gossip] ====

use crate::base_constants::{GOSSIP_MAX_PEERS, GOSSIP_SEED_ADDR};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GossipConfig {
	pub enabled: bool,
	pub addr: String,
	pub discovery: bool,
	pub network_id: Option<String>,
	pub discovery_port: u16,
	pub peers: Vec<String>,
	pub seed: bool,
	pub seed_addr: String,
	// Small-world ring topology (FEDERATION_PLAN §2). Off = legacy flat peers.
	pub ring: bool,
	// Path of the ed25519 peer key file; empty = <data_dir>/peer.key.
	pub identity_path: String,
	// Anti-entropy cadence for contract kerns (FEDERATION_PLAN §4).
	pub sync_interval_secs: u64,
	// Contract ids (hex) to subscribe to on boot.
	pub subscriptions: Vec<String>,
	// Contracts this node hosts/owns.
	pub contracts: Vec<ContractConfig>,
}

// One `[[gossip.contracts]]` table: the policy whose hash is the contract key
// (FEDERATION_PLAN §3). Keys are hex-encoded ed25519 public keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContractConfig {
	pub kind: String,
	pub owners: Vec<String>,
	// "open" | "owners-only" | allowlist implied by non-empty `writers` list.
	pub writers: String,
	pub writer_keys: Vec<String>,
	pub kinds: Vec<String>,
	pub max_entities: u32,
	pub retention_secs: Option<u64>,
}

impl Default for ContractConfig {
	fn default() -> Self {
		Self {
			kind: "signed-crdt-v0".into(),
			owners: Vec::new(),
			writers: "owners-only".into(),
			writer_keys: Vec::new(),
			kinds: Vec::new(),
			max_entities: crate::base_constants::GOSSIP_REMOTE_KERN_ENTITY_CAP as u32,
			retention_secs: None,
		}
	}
}

impl GossipConfig {
	pub fn effective_seed(&self) -> Option<&str> {
		if !self.enabled || !self.seed {
			return None;
		}
		let addr = self.seed_addr.trim();
		(!addr.is_empty()).then_some(addr)
	}

	// The only peer source that runs before any inbound contact; still bounded by GOSSIP_MAX_PEERS.
	pub fn bootstrap_peers(&self) -> Vec<String> {
		if !self.enabled {
			return Vec::new();
		}
		let mut peers = self.peers.clone();
		if let Some(seed) = self.effective_seed() {
			if !peers.iter().any(|p| p == seed) {
				peers.push(seed.to_string());
			}
		}
		peers.truncate(GOSSIP_MAX_PEERS);
		peers
	}

	// A ':' in the id would corrupt the `kern:<id>:<addr>` announce wire format.
	pub fn effective_network_id(&self, generated: &str) -> String {
		match self.network_id.as_deref() {
			Some(id) if !id.is_empty() && !id.contains(':') => id.to_string(),
			Some(id) if !id.is_empty() => {
				tracing::warn!(
					target: "kern.gossip",
					network_id = %id,
					"[gossip] network_id must not contain ':'; falling back to the generated id"
				);
				generated.to_string()
			}
			_ => generated.to_string(),
		}
	}
}

impl Default for GossipConfig {
	fn default() -> Self {
		Self {
			enabled: false,
			addr: "0.0.0.0:7400".into(),
			discovery: true,
			network_id: None,
			discovery_port: 7475,
			peers: Vec::new(),
			// Dialing a public host is opt-in: federation is unauthenticated, so a
			// default-on seed would auto-join a stranger's network.
			seed: false,
			seed_addr: GOSSIP_SEED_ADDR.into(),
			// Ring routing is phase-gated off until a network opts in.
			ring: false,
			identity_path: String::new(),
			sync_interval_secs: 300,
			subscriptions: Vec::new(),
			contracts: Vec::new(),
		}
	}
}

#[cfg(test)]
mod retrieval_tests {
	use super::*;

	#[test]
	fn default_is_disabled_with_expected_field_values() {
		let c = GossipConfig::default();
		assert!(!c.enabled, "gossip is disabled by default");
		assert_eq!(c.addr, "0.0.0.0:7400");
		assert!(
			c.discovery,
			"discovery defaults on (only matters once enabled)"
		);
		assert_eq!(c.discovery_port, 7475);
		assert!(
			c.network_id.is_none(),
			"no pooling id by default — each daemon keeps its unique generated id"
		);
		assert!(c.peers.is_empty(), "no seed peers by default");
		assert!(!c.ring, "ring topology is opt-in (phase 2 switch)");
		assert!(
			c.identity_path.is_empty(),
			"peer key defaults beside the graph it identifies"
		);
		assert_eq!(c.sync_interval_secs, 300);
		assert!(
			c.subscriptions.is_empty(),
			"no boot subscriptions by default"
		);
		assert!(c.contracts.is_empty(), "no hosted contracts by default");
		assert!(
			!c.seed,
			"dialing the public seed is opt-in, never a default"
		);
		assert_eq!(c.seed_addr, GOSSIP_SEED_ADDR);
	}

	#[test]
	fn disabled_gossip_bootstraps_nothing_at_all() {
		// `seed: true` on purpose: the disabled gate, not the seed default, is what
		// must silence this — otherwise the test passes for the wrong reason.
		let c = GossipConfig {
			seed: true,
			peers: vec!["10.0.0.5:7400".into()],
			..GossipConfig::default()
		};
		assert!(!c.enabled);
		assert_eq!(
			c.effective_seed(),
			None,
			"a default daemon must make zero outbound calls"
		);
		assert!(c.bootstrap_peers().is_empty());
	}

	#[test]
	fn enabling_gossip_alone_dials_nothing() {
		let c = GossipConfig {
			enabled: true,
			..GossipConfig::default()
		};
		assert_eq!(
			c.effective_seed(),
			None,
			"turning gossip on must not, by itself, dial the public seed"
		);
		assert!(c.bootstrap_peers().is_empty());
	}

	#[test]
	fn opting_into_the_seed_dials_the_default_addr() {
		let c = GossipConfig {
			enabled: true,
			seed: true,
			..GossipConfig::default()
		};
		assert_eq!(c.effective_seed(), Some(GOSSIP_SEED_ADDR));
		assert_eq!(c.bootstrap_peers(), vec![GOSSIP_SEED_ADDR.to_string()]);
	}

	#[test]
	fn an_explicit_seed_overrides_the_default() {
		let c = GossipConfig {
			enabled: true,
			seed: true,
			seed_addr: "seed.internal:7946".into(),
			..GossipConfig::default()
		};
		assert_eq!(c.effective_seed(), Some("seed.internal:7946"));
		assert!(!c.bootstrap_peers().iter().any(|p| p == GOSSIP_SEED_ADDR));
	}

	#[test]
	fn the_seed_turns_off_while_gossip_stays_on() {
		let mut c = GossipConfig {
			enabled: true,
			seed: false,
			peers: vec!["10.0.0.5:7400".into()],
			..GossipConfig::default()
		};
		assert_eq!(c.effective_seed(), None, "air-gapped LAN never phones out");
		assert_eq!(c.bootstrap_peers(), vec!["10.0.0.5:7400".to_string()]);

		c.seed = true;
		c.seed_addr = "   ".into();
		assert_eq!(
			c.effective_seed(),
			None,
			"a blank seed_addr also disables it"
		);
	}

	#[test]
	fn bootstrap_peers_never_exceed_the_peer_cap() {
		let c = GossipConfig {
			enabled: true,
			peers: (0..GOSSIP_MAX_PEERS + 10)
				.map(|i| format!("10.0.0.{i}:7400"))
				.collect(),
			..GossipConfig::default()
		};
		assert_eq!(c.bootstrap_peers().len(), GOSSIP_MAX_PEERS);
	}

	#[test]
	fn a_seed_already_in_peers_is_not_duplicated() {
		let c = GossipConfig {
			enabled: true,
			seed: true,
			peers: vec![GOSSIP_SEED_ADDR.into()],
			..GossipConfig::default()
		};
		assert_eq!(c.bootstrap_peers(), vec![GOSSIP_SEED_ADDR.to_string()]);
	}

	#[test]
	fn effective_network_id_prefers_a_valid_configured_id() {
		let c = GossipConfig {
			network_id: Some("team-alpha".into()),
			..GossipConfig::default()
		};
		assert_eq!(c.effective_network_id("generated-uuid"), "team-alpha");
	}

	#[test]
	fn effective_network_id_falls_back_when_unset_empty_or_invalid() {
		let mut c = GossipConfig::default();
		assert_eq!(c.effective_network_id("gen"), "gen", "unset -> generated");
		c.network_id = Some(String::new());
		assert_eq!(c.effective_network_id("gen"), "gen", "empty -> generated");
		c.network_id = Some("has:colon".into());
		assert_eq!(
			c.effective_network_id("gen"),
			"gen",
			"':' would corrupt the announce wire format"
		);
	}
}

// ==== [detached_log] ====

use std::process::Stdio;

// One file per spawn arg, so hub and daemon never interleave into one log.
pub fn log_path(log_dir: &Path, arg: &str) -> PathBuf {
	log_dir.join(format!("{}.log", arg.trim_start_matches('-')))
}

fn open(log_dir: &Path, arg: &str) -> std::io::Result<(std::fs::File, std::fs::File)> {
	std::fs::create_dir_all(log_dir)
		.and_then(|()| crate::config::open_private_append(&log_path(log_dir, arg)))
		.and_then(|f| f.try_clone().map(|dup| (f, dup)))
}

/// Append, never truncate: a restart must not erase the log explaining why it
/// restarted. A log we cannot open must not cost us the spawn — fall back to
/// `/dev/null` and say so on the parent's stderr, which is still attached here.
pub fn stdio(log_dir: &Path, arg: &str) -> (Stdio, Stdio) {
	match open(log_dir, arg) {
		Ok((out, err)) => (Stdio::from(out), Stdio::from(err)),
		Err(e) => {
			eprintln!(
				"kern: cannot log to {} ({e}) — the detached child's output is discarded",
				log_path(log_dir, arg).display()
			);
			(Stdio::null(), Stdio::null())
		}
	}
}

#[cfg(test)]
mod serve_tests {
	use super::*;

	#[test]
	fn one_log_file_per_spawn_arg() {
		let dir = Path::new("/tmp/kern-logs");
		assert_eq!(log_path(dir, "hub"), dir.join("hub.log"));
		assert_eq!(
			log_path(dir, "--daemon"),
			dir.join("daemon.log"),
			"the leading dashes are not part of the name"
		);
	}

	#[test]
	fn opening_creates_the_dir_and_appends_rather_than_truncating() {
		let dir = tempfile::tempdir().unwrap();
		let logs = dir.path().join("nested").join("logs");

		{
			use std::io::Write;
			let (mut out, _) = open(&logs, "hub").expect("first open creates the dir");
			out.write_all(b"first\n").unwrap();
		}
		{
			use std::io::Write;
			let (mut out, _) = open(&logs, "hub").expect("reopen");
			out.write_all(b"second\n").unwrap();
		}

		assert_eq!(
			std::fs::read_to_string(log_path(&logs, "hub")).unwrap(),
			"first\nsecond\n",
			"a reopen must not erase what explains the restart"
		);
	}

	#[test]
	fn an_unopenable_log_is_an_error_the_caller_can_see() {
		let dir = tempfile::tempdir().unwrap();
		let blocked = dir.path().join("not-a-dir");
		std::fs::write(&blocked, "i am a file").unwrap();

		assert!(
			open(&blocked, "hub").is_err(),
			"create_dir_all over an existing file must fail, so the fallback is reachable"
		);
	}
}
