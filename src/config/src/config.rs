//! Configuration, whole: the resolved [`Config`] with every section
//! (embed, reason, gnn, graph, hub, ingest, intake, reload, retrieval, serve,
//! tick, watcher, gossip), the `.git`-first root resolution and deep merge of
//! global-then-project TOML in the io half, the tuning presets, secret
//! redirection, and the detached-log plumbing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct HeatConfig {
	pub half_life_secs: u64,
	// Dimensionless heat unit — not a ratio or duration. The only deposit there
	// is: heat measures use, and the tick is not a user (ROADMAP item 32).
	pub deposit_access: f32,
}

impl Default for HeatConfig {
	fn default() -> Self {
		Self {
			half_life_secs: 7 * 24 * 60 * 60,
			deposit_access: 1.0,
		}
	}
}

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
		// KERN_DIR pins the store base (pi's integration sets it to
		// `<root>/.pi/kern`): without it a fresh build silently falls back to
		// `<cwd>/.kern` and every session reads/writes a different store. The
		// installed binary honored it; the source did not — this restores it.
		let kern_dir = std::env::var_os("KERN_DIR")
			.map(PathBuf::from)
			.unwrap_or_else(|| cwd.join(".kern"));
		let mut cfg = Self {
			data_dir: kern_dir
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
		cfg.retrieval.resolve_voice_overrides();
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
		cfg.retrieval.resolve_voice_overrides();
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
			if !url.is_empty() && !llm::is_local_url(url) {
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
		if llm::is_openai_compat(&self.embed.url) {
			if self.embed.num_ctx != 0 && self.embed.num_ctx != llm::EMBED_NUM_CTX {
				out.push(format!(
					"embed.num_ctx = {} is ignored — embed.url ({}) is an OpenAI-compatible /v1 endpoint with no client-side context window",
					self.embed.num_ctx, self.embed.url
				));
			}
			if !self.embed.keep_alive.is_empty() && self.embed.keep_alive != llm::EMBED_KEEP_ALIVE {
				out.push(format!(
					"embed.keep_alive = \"{}\" is ignored — embed.url ({}) is an OpenAI-compatible /v1 endpoint with no keep-alive option",
					self.embed.keep_alive, self.embed.url
				));
			}
		}
		if llm::is_openai_compat(&self.reason.url) {
			if self.reason.num_ctx != 0 && self.reason.num_ctx != llm::REASON_NUM_CTX {
				out.push(format!(
					"reason.num_ctx = {} is ignored — reason.url ({}) is an OpenAI-compatible /v1 endpoint with no client-side context window",
					self.reason.num_ctx, self.reason.url
				));
			}
			if !self.reason.keep_alive.is_empty() && self.reason.keep_alive != llm::REASON_KEEP_ALIVE {
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
		self.wsl_loopback_warnings_for(llm::is_wsl())
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
			if !url.is_empty() && llm::is_loopback_url(url) {
				out.push(format!(
					"{label} ({url}) is loopback, but kern is running under WSL — a Linux 127.0.0.1 does not reach a Windows-host Ollama. Pin the WSL2 gateway IP instead (e.g. the host side of /etc/resolv.conf, or `ip route show default`)"
				));
			}
		}
		out
	}
}

#[cfg(test)]
#[path = "tests/config_test.rs"]
mod config_tests;

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
			num_ctx: llm::EMBED_NUM_CTX,
			keep_alive: llm::EMBED_KEEP_ALIVE.into(),
		}
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

impl Default for ReasonConfig {
	fn default() -> Self {
		Self {
			url: DEFAULT_REASON_URL.into(),
			model: DEFAULT_REASON_MODEL.into(),
			key: String::new(),
			timeout_secs: llm::DEFAULT_REASON_TIMEOUT_SECS,
			num_ctx: llm::REASON_NUM_CTX,
			keep_alive: llm::REASON_KEEP_ALIVE.into(),
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

// Canonical GNN defaults — config owns them so it never reaches up to `gnn`;
// `gnn::propagate::GnnConfig::defaults()` reads these down here.
pub const DEFAULT_SELF_WEIGHT: f64 = 0.6;
pub const DEFAULT_MIN_WEIGHT: f64 = 0.01;
pub const DEFAULT_MIN_THOUGHTS: usize = 128;
pub const DEFAULT_TRAIN_EPOCHS: usize = 24;
pub const DEFAULT_TRAIN_LEARNING_RATE: f64 = 0.01;

impl Default for GnnConfig {
	fn default() -> Self {
		Self {
			self_weight: DEFAULT_SELF_WEIGHT,
			min_weight: DEFAULT_MIN_WEIGHT,
			min_thoughts: DEFAULT_MIN_THOUGHTS,
			train_epochs: DEFAULT_TRAIN_EPOCHS,
			train_learning_rate: DEFAULT_TRAIN_LEARNING_RATE,
		}
	}
}

// ==== [graph] ====

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
			// never forgets.
			// disk_threshold 0 = every store-backed graph loads its ANN indexes
			// from mmap'd DiskANN snapshots (RECALL_PLAN F4). The item-75 crash
			// window that kept it disabled is closed: build_and_save swaps a
			// fully-fsynced staging dir in one rename, and open() falls back to
			// the in-RAM index on any mismatch. The epoch stamp makes an
			// unchanged store load in ~ms instead of rebuilding three HNSW
			// indexes (the ~4.5s per-CLI-invocation tax that blew pi's tool
			// timeouts).
			max_kerns: 128,
			max_ledger_entries: 10_000,
			disk_threshold: 0,
		}
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

use base::base_constants::INGEST_DEDUP_THRESHOLD;
use base::base_types::{EntityKind, Source};
use ingest_config::ReviewPolicy;

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
	/// Enable the pre-ingestion noise filter. Default: true.
	#[serde(default = "default_filter_enabled")]
	pub filter_enabled: bool,
	/// Custom filter patterns. Empty = use built-in defaults.
	#[serde(default)]
	pub filter_patterns: Vec<String>,
}

fn default_dedup_threshold_by_kind() -> [Option<f64>; EntityKind::Conclusion as usize + 1] {
	[None; EntityKind::Conclusion as usize + 1]
}

const fn default_filter_enabled() -> bool {
	true
}

impl Default for IngestConfig {
	fn default() -> Self {
		Self {
			dedup_threshold: INGEST_DEDUP_THRESHOLD,
			dedup_threshold_by_kind: default_dedup_threshold_by_kind(),
			review_policy: ReviewPolicy::new(),
			filter_enabled: default_filter_enabled(),
			filter_patterns: Vec::new(),
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
		ingest_config::Config {
			dedup_threshold: self.dedup_threshold,
			dedup_threshold_by_kind: self.dedup_threshold_by_kind,
			filter_enabled: self.filter_enabled,
			filter_patterns: self.filter_patterns.clone(),
			..Default::default()
		}
		.validate()
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
		ingest_config::valid_until_from_retention(self.retention_secs)?;
		Ok(())
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

use base::base_constants as constants;

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
	// Voice kill switches — env-var overridable so each retrieval voice can be
	// disabled independently at runtime without recompiling or config changes.
	// See `resolve_voice_overrides()`.
	pub voice_vector_enabled: bool,
	pub voice_lexical_enabled: bool,
	pub voice_graph_enabled: bool,
	pub voice_pagerank_enabled: bool,
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
			// Exact-term matches float above embedding neighbours (RECALL_PLAN F2c):
			// the bonus is `weight * bm25/max_bm25`, so a literal match gains up to
			// 0.5 while a corpus with no verbatim terms is unaffected.
			lexical_top_boost: 0.5,
			pagerank_enabled: true,
			pagerank_damping: 0.85,
			pagerank_iters: 25,
			pagerank_top_k: 100,
			voice_vector_enabled: true,
			voice_lexical_enabled: true,
			voice_graph_enabled: true,
			voice_pagerank_enabled: true,
		}
	}
}

impl RetrievalConfig {
	/// Read `KERN_VOICE_VECTOR`, `KERN_VOICE_LEXICAL`, `KERN_VOICE_GRAPH`, and
	/// `KERN_VOICE_PAGERANK` env vars and override the matching field. Each env
	/// value "0", "false", or "off" (case-insensitive) disables the voice;
	/// anything else (or absent) leaves the field unchanged.
	pub fn resolve_voice_overrides(&mut self) {
		let voice = |var: &str| -> Option<bool> {
			let raw = std::env::var(var).ok()?;
			let lower = raw.trim().to_lowercase();
			Some(!matches!(lower.as_str(), "0" | "false" | "off"))
		};
		if let Some(v) = voice("KERN_VOICE_VECTOR") {
			self.voice_vector_enabled = v;
		}
		if let Some(v) = voice("KERN_VOICE_LEXICAL") {
			self.voice_lexical_enabled = v;
		}
		if let Some(v) = voice("KERN_VOICE_GRAPH") {
			self.voice_graph_enabled = v;
		}
		if let Some(v) = voice("KERN_VOICE_PAGERANK") {
			self.voice_pagerank_enabled = v;
		}
	}

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

// ==== [tick] ====

use base::base_constants::{
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
		ingest_config::valid_until_from_retention(self.retention_secs)?;
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

// ==== [gossip] ====

use base::base_constants::{GOSSIP_MAX_PEERS, GOSSIP_SEED_ADDR};

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
			max_entities: base::base_constants::GOSSIP_REMOTE_KERN_ENTITY_CAP as u32,
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
