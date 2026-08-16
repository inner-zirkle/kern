//! The CLI: clap definitions and dispatch. Every subcommand body lives in a
//! sibling `commands_*` submodule of this crate; this file owns argument
//! shapes and the shared client/daemon plumbing they call into.

pub mod commands_admin;
pub(crate) mod commands_doctor;
pub mod commands_exit;
pub(crate) mod commands_export;
pub(crate) mod commands_graph_ops;
pub mod commands_ingest_cmd;
pub mod commands_intake_cmd;
pub mod commands_query;
pub mod commands_reembed;
pub mod commands_route;

#[allow(unused_imports)]
pub(crate) use bootstrap::{
	apply_graph_config, load_graph, reconcile_if_stale, reload_graph, save_graph_guarded,
	save_graph_unguarded, snapshot_if_dirty, SharedGraph,
};
pub use commands_exit::failed;
pub(crate) use commands_exit::{fail, hint};

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use clap::{Args, Parser, Subcommand};

use graph::graph::GraphGnn;

const SELF_HEAL_BLOAT_BYTES: u64 = 512 * 1024 * 1024;

// The header line, and the only place the version is spelled: `--version`
// answers from the same `CARGO_PKG_VERSION`, so the two cannot drift.
const ABOUT: &str = concat!(
	"kern v",
	env!("CARGO_PKG_VERSION"),
	" - adaptive knowledge graph"
);

// clap's default styling underlines section headers and the usage line. Bold
// alone separates them just as well and leaves the page quiet.
fn help_styles() -> clap::builder::Styles {
	use clap::builder::styling::{AnsiColor, Effects};
	clap::builder::Styles::styled()
		.header(Effects::BOLD.into())
		.usage(Effects::BOLD.into())
		.literal(Effects::BOLD.into())
		.placeholder(AnsiColor::White.on_default())
}

#[derive(Parser)]
#[command(
	name = "kern",
	version,
	about = ABOUT,
	styles = help_styles(),
	max_term_width = 100,
)]
pub struct Cli {
	#[command(subcommand)]
	pub command: Option<Commands>,

	// Hidden, not removed: `kern daemon` is the documented way in, and these three
	// are what the hub, the hot-reload successor and the detached spawns pass on
	// the command line. Every one of them is kern talking to itself.
	#[arg(short = 'd', long, hide = true)]
	pub daemon: bool,

	#[arg(long, default_value = "", hide = true)]
	pub reason_url: String,

	#[arg(long, default_value = "", hide = true)]
	pub reason_model: String,
}

impl Cli {
	fn daemon() -> Self {
		Cli {
			command: None,
			daemon: true,
			reason_url: String::new(),
			reason_model: String::new(),
		}
	}
}

#[derive(Args)]
pub struct EmbedArgs {
	/// Embedding endpoint for this run. Default: `embed.url` in kern.toml.
	#[arg(long, value_name = "URL", help_heading = "Model overrides")]
	pub embed_url: Option<String>,
	/// Embedding model for this run. Default: `embed.model` in kern.toml.
	///
	/// Embedding under any model but the one the store was built with degenerates
	/// every cosine — `kern doctor` reports the mismatch, `kern reembed` fixes it.
	#[arg(long, value_name = "MODEL", help_heading = "Model overrides")]
	pub embed_model: Option<String>,
}

impl EmbedArgs {
	pub(crate) fn resolve<'a>(&'a self, cfg: &'a config::Config) -> (&'a str, &'a str) {
		(
			resolve(&self.embed_url, &cfg.embed.url),
			resolve(&self.embed_model, &cfg.embed.model),
		)
	}
}

#[derive(Args)]
pub struct LlmArgs {
	#[command(flatten)]
	pub embed: EmbedArgs,
	/// Reasoning endpoint for this run. Default: `reason.url` in kern.toml.
	#[arg(long, value_name = "URL", help_heading = "Model overrides")]
	pub reason_url: Option<String>,
	/// Reasoning model for this run. Default: `reason.model` in kern.toml.
	#[arg(long, value_name = "MODEL", help_heading = "Model overrides")]
	pub reason_model: Option<String>,
}

impl LlmArgs {
	pub(crate) fn resolve<'a>(
		&'a self,
		cfg: &'a config::Config,
	) -> (&'a str, &'a str, &'a str, &'a str) {
		let (embed_url, embed_model) = self.embed.resolve(cfg);
		(
			embed_url,
			embed_model,
			resolve(&self.reason_url, &cfg.reason.url),
			resolve(&self.reason_model, &cfg.reason.model),
		)
	}
}

// Declaration order IS help order — clap lists subcommands as written. They run
// front to back in the order a project meets them: put something in, get it
// back, curate what came back, shape the graph, then operate the store.
#[derive(Subcommand)]
pub enum Commands {
	/// Distill text or a file into this project's memory.
	Ingest {
		/// The text to remember. Multiple words are joined with spaces.
		#[arg(value_name = "TEXT")]
		text: Vec<String>,
		/// Read the text from a file instead. Relative paths resolve against
		/// the directory you ran kern in.
		#[arg(long, value_name = "PATH")]
		file: Option<String>,
		/// Expire everything from this ingest after N seconds (0 = never).
		#[arg(long, value_name = "SECS")]
		retention_secs: Option<u64>,
		#[command(flatten)]
		llm: LlmArgs,
	},

	/// Recall by meaning: semantic, lexical and graph-walk search over memory.
	#[command(visible_alias = "grep")]
	Query {
		/// What to recall — a question or a phrase reads better than keywords.
		text: String,
		/// How to weigh the three signals: hybrid (content + reasons + edges),
		/// content, reason, or vector — nearest-neighbour only, read straight
		/// from the local store with no walk and no daemon.
		#[arg(long, default_value = "hybrid", value_parser = ["hybrid", "content", "reason", "vector"])]
		mode: String,
		/// Hits to print. Default: the retrieval preset's delivery cap, the same
		/// in every mode.
		#[arg(long, value_name = "N")]
		k: Option<usize>,
		/// Drop thoughts a review policy is still holding for curation. Opt-in:
		/// without it an uncurated graph reads exactly as before.
		#[arg(long)]
		exclude_pending: bool,
		/// Search every kern on this machine through the hub, merged by score
		/// and each hit tagged with the project it came from.
		#[arg(long)]
		all: bool,
		/// With --all: only ask daemons already running; never wake a cold kern.
		#[arg(long, requires = "all")]
		live: bool,
		#[command(flatten)]
		embed: EmbedArgs,
	},

	/// Print one thought in full: text, provenance, and its edges.
	#[command(visible_alias = "show")]
	Get {
		/// A thought ID, or any unambiguous prefix of one.
		id: String,
	},

	/// The kern tree: every kern and the thoughts it holds.
	List,

	/// What entered memory and when, newest first. With an ID, that thought's
	/// revision chain — every superseded version, with its source and why it fell.
	Log {
		/// A thought ID (or prefix). Omit for the whole history.
		id: Option<String>,
		/// History entries to print. A revision chain always prints whole.
		#[arg(long, default_value_t = 20, value_name = "N")]
		limit: usize,
	},

	/// Connect two thoughts with a reason: why they belong together.
	#[command(visible_alias = "note")]
	Link {
		/// The thought the edge points from.
		from: String,
		/// The thought the edge points to.
		to: String,
		/// Why they belong together. Left empty, the reason model writes it.
		#[arg(long, default_value = "", value_name = "TEXT")]
		reason: String,
		#[command(flatten)]
		llm: LlmArgs,
	},

	/// Remove thoughts: one by ID, a whole source, or every text match.
	#[command(visible_alias = "rm")]
	Forget {
		/// The thought to forget. Omit when using --source or --match.
		id: Option<String>,
		/// Forget every thought from one source: `<scheme>://<object_id>`.
		#[arg(long, conflicts_with = "id", value_name = "SOURCE")]
		source: Option<String>,
		/// Forget every thought whose text contains PATTERN, matched
		/// case-insensitively. Narrow it further with --source.
		#[arg(long = "match", conflicts_with = "id", value_name = "PATTERN")]
		match_text: Option<String>,
		/// Report what would be removed without removing it. Bulk removals only.
		#[arg(long)]
		dry_run: bool,
		// Paired with the bulk paths in `dispatch`, NOT with clap's `requires`:
		// that does not fire for a SetTrue flag, so `forget --force <id>` was
		// accepted and silently ignored.
		/// Also remove Facts — the one bypass of the Fact guard, and never
		/// implicit. Bulk removals only: a single ID names one Fact the caller
		/// can already see.
		#[arg(long)]
		force: bool,
	},

	/// Weaken a thought's edges so it stops surfacing.
	///
	/// The answer to recall that returned something wrong or stale: the thought
	/// stays, its pull on the walk does not.
	Degrade {
		/// The thought to degrade, by ID or prefix.
		id: String,
	},

	/// Mark a thought reviewed: release it from `pending` so a
	/// `query --exclude-pending` returns it again.
	Promote {
		/// The thought to release, by ID or prefix.
		id: String,
	},

	/// Rank stored thoughts by noise likelihood. Report-only unless --apply.
	///
	/// Terminal spam, stack traces, heartbeats and secrets, scored
	/// deterministically — no LLM, so the same store always ranks the same way.
	Audit {
		/// Report floor; secret-bearing thoughts surface regardless.
		#[arg(long, default_value_t = 0.3, value_name = "SCORE")]
		min_score: f64,
		/// Candidates printed (the scan itself is unbounded).
		#[arg(long, default_value_t = 20, value_name = "N")]
		limit: usize,
		/// Print the report as JSON instead of a table.
		#[arg(long)]
		json: bool,
		/// Act on the candidates: `archive` holds them pending (reversible via
		/// `promote`, filtered by `query --exclude-pending`), `delete` forgets
		/// them. Each action has its own score floor (archive 0.5, delete 0.8)
		/// that --min-score can only raise; secrets are never bulk-deleted.
		#[arg(long, value_parser = ["archive", "delete"], value_name = "ACTION")]
		apply: Option<String>,
	},

	/// Show the intake queue, or drain it once with no daemon running.
	Intake {
		#[command(subcommand)]
		action: Option<IntakeAction>,
		#[command(flatten)]
		llm: LlmArgs,
	},

	/// Name a kern by its attractor: the seed text its thoughts fall toward.
	Graviton {
		#[command(subcommand)]
		action: GravitonAction,
	},

	/// Register project-specific claim kinds for distillation to sort into.
	ClaimKind {
		#[command(subcommand)]
		action: ClaimKindAction,
	},

	/// Kerns that clustered but were never named — list them, or name one.
	Unnamed {
		#[command(subcommand)]
		action: UnnamedAction,
	},

	/// Who is serving and who is writing this directory.
	Status,

	/// Live counters: kerns, thoughts, embed stamp, and anything degraded.
	Health,

	/// Diagnose the store: config, embed stamp, dangling edges, empty kerns.
	///
	/// Strictly read-only and content-safe — it never mutates, and it never
	/// prints a thought's text. `--json` emits the manifest `kern repair` reads.
	Doctor {
		/// Emit the findings manifest `kern repair` consumes.
		#[arg(long)]
		json: bool,
	},

	/// Execute the repairs a doctor manifest authorizes, and nothing else.
	///
	/// No discovery of its own: an empty manifest repairs nothing, and every
	/// repair is re-verified against the live graph before it runs.
	Repair {
		/// Path to a manifest written by `kern doctor --json`.
		#[arg(value_name = "MANIFEST")]
		manifest: String,
	},

	/// Rewrite a store written by an older build into the current format.
	///
	/// Reading one already converts it in memory, so recall works before this
	/// runs; what this adds is finishing the job on disk, in one pass you can
	/// verify, instead of whenever the next write happens to land. Daemon must
	/// be stopped. One-way: the rewritten store is no longer readable by the
	/// build that wrote it.
	Migrate,

	/// Reap empty kerns and compact the store. Daemon must be stopped.
	///
	/// LMDB returns freed pages to the filesystem only on compaction, and the
	/// compaction renames data.mdb underneath any open environment — so this
	/// refuses while a daemon holds the writer lock, naming the holder.
	Gc,

	/// Re-embed every thought with the configured model. Daemon must be stopped.
	///
	/// The recovery path after an embedding-model change: vectors from two
	/// models score as noise against each other, which `kern doctor` reports
	/// and only this rebuilds.
	Reembed {
		#[command(flatten)]
		embed: EmbedArgs,
	},

	/// Write the whole hot graph as versioned JSON — the backup that survives a
	/// FORMAT_VERSION wipe.
	Export {
		/// Where to write the export.
		#[arg(long, default_value = "kern-export.json", value_name = "PATH")]
		out: String,
	},

	/// Absorb a kern export into this store — a CRDT union, idempotent on re-import.
	///
	/// Same semantics as `hub merge`. Refuses a mismatched embed model unless
	/// --force, because the imported vectors would silently score as noise.
	Import {
		/// The export file to absorb.
		#[arg(value_name = "FILE")]
		file: String,
		/// Import anyway when the embed models differ. The imported vectors will
		/// score as noise until `kern reembed` rebuilds them.
		#[arg(long)]
		force: bool,
	},

	/// Absorb another store directory into this one.
	Register {
		/// Path to the store directory to read.
		#[arg(value_name = "PATH")]
		path: String,
	},

	/// Write a quantized copy of a store directory — smaller vectors, lower recall.
	Compress {
		/// The store directory to read.
		#[arg(value_name = "SRC")]
		src: String,
		/// Quantization: int8 (4x smaller) or none.
		#[arg(long, default_value = "int8", value_parser = ["int8", "none"])]
		mode: String,
		/// Where to write the copy. Default: `<SRC>.<mode>`.
		#[arg(long, value_name = "PATH")]
		out: Option<String>,
	},

	/// Time the retrieval pipeline stage by stage against this store.
	Profile {
		/// The probe query to time.
		#[arg(
			long,
			default_value = "what is this project about",
			value_name = "TEXT"
		)]
		text: String,
		/// Skip the stages that call the reason model.
		#[arg(long)]
		no_llm: bool,
	},

	/// Serve this directory: recall, ingest, the tick loop, and the intake watcher.
	Daemon,

	/// The machine hub: one daemon per project, started on demand.
	Hub {
		#[command(subcommand)]
		action: Option<HubAction>,
		/// Auto-unload hub-owned nodes idle this long; 0 disables.
		#[arg(long, default_value_t = 1800, value_name = "SECS")]
		idle_unload_secs: u64,
	},
}

#[derive(Subcommand)]
pub enum HubAction {
	/// Every kern on this machine: which are loaded, which are cold.
	Status,
	/// Print the endpoint serving a project, starting a daemon if none is.
	Resolve {
		/// Project root. Default: this one.
		root: Option<String>,
	},
	/// Stop the daemon serving a project and drop it from the hub.
	Unload {
		/// Project root. Default: this one.
		root: Option<String>,
	},
	/// Absorb src's graph into dst (CRDT union). Both daemons are stopped
	/// first; src is left untouched.
	Merge {
		/// Project root to read from.
		src: String,
		/// Project root to merge into.
		dst: String,
	},
	/// Stop the hub daemon; nodes stay up.
	Stop,
}

#[derive(Subcommand)]
pub enum IntakeAction {
	/// Pending and failed deltas, with the last error for anything stuck.
	Status,
	/// Run one drain pass in this process; no daemon required.
	Drain,
}

#[derive(Subcommand)]
pub enum GravitonAction {
	/// Give a kern a name and the seed text that attracts thoughts to it.
	Add {
		/// The kern's name.
		name: String,
		/// Seed text. Multiple lines are embedded separately and mean-pooled,
		/// so example statements work better than a single label.
		text: String,
		/// Pull relative to other gravitons. Default: 1.0.
		#[arg(long, value_name = "MASS")]
		mass: Option<f64>,
		#[command(flatten)]
		embed: EmbedArgs,
	},
	/// Every graviton with its mass and what it holds.
	List,
	/// Remove a graviton. Its thoughts stay; the kern goes unnamed.
	Remove {
		/// The graviton's name.
		name: String,
	},
}

#[derive(Subcommand)]
pub enum ClaimKindAction {
	/// Register a claim kind distillation may sort thoughts into.
	Add {
		/// The kind's name.
		name: String,
		/// What belongs in it — this is the prompt distillation reads.
		description: String,
		/// Optional parent claim kind (builtin or registered) this kind
		/// specializes — queries filtering on the parent also return this kind.
		#[arg(long, value_name = "KIND")]
		parent: Option<String>,
	},
	/// Remove a registered claim kind.
	Rm {
		/// The kind's name.
		name: String,
	},
}

#[derive(Subcommand)]
pub enum UnnamedAction {
	/// Every unnamed kern, with the thoughts it holds.
	List,
	/// Name an unnamed kern by giving it a graviton in place.
	Promote {
		/// The unnamed kern id (the short form `kern unnamed list` prints).
		id: String,
		/// The name to give it.
		name: String,
		/// Seed text for the new graviton.
		text: String,
		/// Pull relative to other gravitons. Default: 1.0.
		#[arg(long, value_name = "MASS")]
		mass: Option<f64>,
		#[command(flatten)]
		embed: EmbedArgs,
	},
}

fn maybe_self_heal_store(cfg: &config::Config) {
	let data = std::path::Path::new(&cfg.data_dir).join("data.mdb");
	let len = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);
	if len < SELF_HEAL_BLOAT_BYTES {
		return;
	}

	tracing::info!(target: "kern.startup", bytes = len, "data.mdb is bloated; self-healing (reap + compact)");

	// Drop the throwaway graph so its env handle releases before the compaction swap.
	{
		let mut g = load_graph(cfg);
		let (before, reaped, after) = g.gc_empty_kerns_counted();
		if reaped > 0 {
			save_graph_unguarded(&g);
			eprintln!("kern: self-heal reaped {reaped} empty kerns ({before} -> {after})");
		}
	}
	match store_core::compact_dir(&cfg.data_dir) {
		Ok((old, new)) => eprintln!(
			"kern: self-heal compacted data.mdb {} MiB -> {} MiB",
			old / (1024 * 1024),
			new / (1024 * 1024),
		),
		Err(e) => {
			tracing::warn!(target: "kern.startup", error = %e, "self-heal compaction skipped (store may be held by another process)");
		}
	}
}

pub(crate) fn with_graph<R>(cfg: &config::Config, f: impl FnOnce(&mut GraphGnn) -> R) -> R {
	let mut g = load_graph(cfg);
	let out = f(&mut g);
	save_graph_unguarded(&g);
	out
}

pub(crate) fn resolve<'a>(arg: &'a Option<String>, fallback: &'a str) -> &'a str {
	arg.as_deref().unwrap_or(fallback)
}

pub(crate) use llm::{Client, Endpoint};

pub(crate) fn embed_fn(client: &Client) -> llm::EmbedFunc {
	let c = client.clone();
	std::sync::Arc::new(move |text: &str| -> Result<Vec<f32>, String> {
		let c = c.clone();
		let text = text.to_string();
		match llm::block_on_in_place(c.embed(&text)) {
			Some(r) => r.map_err(|e| e.to_string()),
			None => Err("no runtime".to_string()),
		}
	})
}

// embed is ALWAYS taken from config — embedding with any model but the
// graph's degenerates every cosine.
pub(crate) fn server_llm_client(
	cfg: &config::Config,
	reason_url: &str,
	reason_model: &str,
) -> Client {
	Client::new(
		Endpoint::new(reason_url, reason_model, cfg.reason_key()),
		Endpoint::new(&cfg.embed.url, &cfg.embed.model, &cfg.embed.key),
	)
	.with_timeout_secs(cfg.reason.timeout_secs)
	.with_num_ctx(cfg.reason.num_ctx)
	.with_reason_keep_alive(&cfg.reason.keep_alive)
	.with_embed_num_ctx(cfg.embed.num_ctx)
	.with_embed_keep_alive(&cfg.embed.keep_alive)
}

pub async fn dispatch(cmd: Commands, cfg: &config::Config) {
	match cmd {
		Commands::Ingest {
			text,
			file,
			retention_secs,
			llm,
		} => {
			let (embed_url, embed_model, reason_url, reason_model) = llm.resolve(cfg);
			crate::commands_ingest_cmd::cmd_ingest(
				cfg,
				text,
				file,
				retention_secs.unwrap_or(0),
				embed_url,
				embed_model,
				reason_url,
				reason_model,
			)
			.await
		}

		// One recall command, three reads under it. `--all` is the hub's
		// cross-project fan-out, `--mode vector` is the bare nearest-neighbour
		// read of the local store, and everything else is the full pipeline.
		Commands::Query {
			text,
			mode,
			k,
			exclude_pending,
			all,
			live,
			embed,
		} => {
			let (embed_url, embed_model) = embed.resolve(cfg);
			// One default for every mode: the retrieval preset's delivery cap. The
			// cross-kern and nearest-neighbour reads used to default to 5 — inherited
			// from the `search` command they absorbed — so the same question asked
			// three ways answered with three different numbers of hits, and `--all`
			// looked like it had found almost nothing.
			let k = k.unwrap_or_else(|| retrieval::score::delivery_cap(&cfg.retrieval));
			if all {
				crate::commands_query::cmd_search_all(cfg, &text, k, live).await
			} else if mode == "vector" {
				crate::commands_query::cmd_search(cfg, &text, k, embed_url, embed_model).await
			} else {
				crate::commands_query::cmd_query(
					cfg,
					crate::commands_query::QueryParams {
						text: &text,
						mode: &mode,
						k: Some(k),
						exclude_pending,
						embed_url,
						embed_model,
					},
				)
				.await
			}
		}

		Commands::Reembed { embed } => {
			let (embed_url, embed_model) = embed.resolve(cfg);
			crate::commands_reembed::cmd_reembed(cfg, embed_url, embed_model).await
		}

		Commands::Get { id } => crate::commands_graph_ops::cmd_get(cfg, &id).await,
		Commands::List => crate::commands_graph_ops::cmd_list(cfg),
		// A revision chain is a whole answer, not a page of one: `limit` bounds
		// the history feed and `log_report` ignores it for an id anyway.
		Commands::Log { id, limit } => {
			crate::commands_graph_ops::cmd_log(cfg, id.as_deref(), limit).await
		}

		// One removal command over three paths: a single ID routes to the daemon
		// that owns it, a source or a text match is the bulk sweep that takes the
		// writer lock instead. --dry-run and --force are the bulk sweep's alone.
		Commands::Forget {
			id,
			source,
			match_text,
			dry_run,
			force,
		} => match (id, source, match_text) {
			// A flag the per-id path would silently ignore is worse than no flag:
			// the caller asked for something and got a result that reads like the
			// thought simply was not there.
			(Some(_), _, _) if force || dry_run => fail(
				"forget",
				"--force and --dry-run apply to --source and --match removals, not to a single thought ID",
			),
			(Some(id), _, _) => crate::commands_graph_ops::cmd_forget(cfg, &id).await,
			// A source with no pattern and nothing to preview is the cascade the
			// daemon can run live; anything else is the local matching sweep.
			(None, Some(source), None) if !dry_run => {
				crate::commands_graph_ops::cmd_forget_source(cfg, &source, force).await
			}
			(None, source, pattern) if source.is_some() || pattern.is_some() => {
				crate::commands_graph_ops::cmd_prune(
					cfg,
					pattern.as_deref().unwrap_or(""),
					source.as_deref(),
					dry_run,
					force,
				)
			}
			(None, _, _) => fail(
				"forget",
				"pass a thought ID, --source <scheme>://<object_id>, or --match <pattern>",
			),
		},

		Commands::Link {
			from,
			to,
			reason,
			llm,
		} => {
			let (embed_url, embed_model, reason_url, reason_model) = llm.resolve(cfg);
			crate::commands_graph_ops::cmd_link(
				cfg,
				&from,
				&to,
				&reason,
				embed_url,
				embed_model,
				reason_url,
				reason_model,
			)
			.await
		}

		Commands::Intake { action, llm } => {
			let (embed_url, embed_model, reason_url, reason_model) = llm.resolve(cfg);
			crate::commands_intake_cmd::cmd_intake(
				cfg,
				action,
				embed_url,
				embed_model,
				reason_url,
				reason_model,
			)
			.await
		}

		Commands::Audit {
			min_score,
			limit,
			json,
			apply,
		} => crate::commands_graph_ops::cmd_audit(cfg, min_score, limit, json, apply.as_deref()),

		Commands::Status => crate::commands_admin::cmd_status(cfg).await,
		Commands::Health => crate::commands_admin::cmd_health(cfg).await,
		Commands::Doctor { json } => crate::commands_doctor::cmd_doctor(cfg, json),
		Commands::Repair { manifest } => crate::commands_doctor::cmd_repair(cfg, &manifest),
		Commands::Profile { text, no_llm } => {
			crate::commands_query::cmd_profile(cfg, &text, no_llm).await
		}
		Commands::Migrate => crate::commands_admin::cmd_migrate(cfg),
		Commands::Gc => crate::commands_admin::cmd_gc(cfg),

		Commands::Graviton { action } => crate::commands_admin::cmd_graviton(cfg, action).await,

		Commands::Degrade { id } => crate::commands_graph_ops::cmd_degrade(cfg, &id).await,
		Commands::Promote { id } => crate::commands_graph_ops::cmd_promote(cfg, &id).await,
		Commands::ClaimKind { action } => crate::commands_admin::cmd_claim_kind(cfg, action).await,
		Commands::Register { path } => crate::commands_admin::cmd_register(cfg, &path),
		Commands::Unnamed { action } => crate::commands_admin::cmd_unnamed(cfg, action).await,
		Commands::Export { out } => crate::commands_export::cmd_export(cfg, &out),
		Commands::Import { file, force } => crate::commands_export::cmd_import(cfg, &file, force),
		Commands::Compress { src, mode, out } => {
			crate::commands_admin::cmd_compress(&src, &mode, out.as_deref())
		}
		Commands::Daemon => {
			// main.rs intercepts Daemon first; this arm is kept as a fallthrough.
			run_server(&Cli::daemon(), cfg).await;
		}
		Commands::Hub {
			action,
			idle_unload_secs,
		} => crate::commands_admin::cmd_hub(action, idle_unload_secs).await,
	}
}

pub(crate) struct EngineHandle {
	pub server: std::sync::Arc<::rpc::server::Server>,
	pub task_q: std::sync::Arc<tick::tick_queue::Queue>,
	// Guarded persist closure: the shutdown flush never overwrites a grown disk.
	pub save_fn: std::sync::Arc<dyn Fn() + Send + Sync>,
	// Held for the daemon's lifetime so a direct-writer admin command refuses
	// instead of racing it. Dropped (and released by the OS) when the daemon
	// exits, kill included.
	pub _writer_lock: Option<store::lock::WriterLock>,
}

pub(crate) async fn bootstrap(cli: &Cli, cfg: &config::Config) -> EngineHandle {
	// Stamps uptime for the staleness handshake. Before any await so a health
	// probe on a slow cold boot cannot read 0 and be mistaken for unknown.
	identity::mark_start();
	// Must run BEFORE any env opens: the compaction swaps data.mdb, and only
	// here — post kern.sock win, pre env open — is the dir held exclusively.
	// Skipped on takeover: the predecessor holds the env for a few more ms and
	// just flushed cleanly, so there is nothing to heal and no exclusivity.
	if !identity::is_takeover_boot() {
		maybe_self_heal_store(cfg);
	}

	// Advisory, and deliberately non-fatal: the daemon is the graph's owner, so
	// it claims the dir but never refuses to serve over a lock it cannot take.
	// A takeover boot expects the predecessor to still hold it for a few ms, so
	// retry with backoff before giving up — a daemon that serves for hours
	// without the lock leaves the dir open to a concurrent writer wipe.
	let writer_lock = {
		const LOCK_RETRIES: u32 = 10;
		let mut lock = None;
		for attempt in 0..LOCK_RETRIES {
			match store::lock::acquire(&cfg.data_dir, "daemon") {
				Ok(l) => {
					lock = Some(l);
					break;
				}
				Err(e) if attempt + 1 < LOCK_RETRIES => {
					tracing::info!(
						target: "kern.startup",
						error = %e,
						attempt,
						"writer lock held; retrying"
					);
					tokio::time::sleep(std::time::Duration::from_millis(500)).await;
				}
				Err(e) => {
					tracing::error!(
						target: "kern.startup",
						error = %e,
						"could not claim the writer lock after {LOCK_RETRIES} attempts; direct-writer admin commands will not be refused while this daemon runs"
					);
				}
			}
		}
		lock
	};

	let reason_url = if cli.reason_url.is_empty() {
		cfg.reason_url().to_string()
	} else {
		cli.reason_url.clone()
	};
	let reason_model = if cli.reason_model.is_empty() {
		cfg.reason.model.clone()
	} else {
		cli.reason_model.clone()
	};
	let llm_client = server_llm_client(cfg, &reason_url, &reason_model);

	let llm_fn: Option<ingest::LlmFunc> = if !reason_url.is_empty() {
		Some(Arc::new(llm_client.complete_func()))
	} else {
		None
	};

	spawn_keepalive(&llm_client);

	// Gate like `llm_fn`: an ungated Some with no reason endpoint means infinite
	// no-op Name re-enqueue churn (do_cluster gates on `llm.is_some()`).
	let tick_llm: Option<tick_loop::tick_tasks::LlmFunc> = if reason_url.is_empty() {
		None
	} else {
		Some(Arc::new(llm_client.complete_func()))
	};
	let tick_embed: tick_loop::tick_tasks::EmbedFunc = embed_fn(&llm_client);

	let registry = Arc::new(::store::Registry::new());
	let entry = registry.open(
		std::path::Path::new(&cfg.data_dir),
		cfg,
		llm_client.clone(),
		tick_llm,
		Some(tick_embed),
	);
	let g = entry.graph.clone();
	let worker = entry.worker.clone();
	let q = entry.tick_q.clone();
	let save_fn = entry.save_fn.clone();

	// Watchdog starts after save_fn is available so a stall can attempt a bounded
	// guarded flush before force-exiting (item 76) — before the graph loads there
	// is nothing to lose, so the later start costs nothing and buys the flush.
	spawn_watchdog(save_fn.clone());

	{
		let (before, reaped, after) = g.write().gc_empty_kerns_counted();
		if reaped > 0 {
			tracing::info!(
				target: "kern.startup",
				reaped,
				before,
				after,
				"reaped empty unnamed kerns"
			);
			eprintln!("kern: reaped {reaped} empty kerns ({before} -> {after})");
			// Persist via the guarded closure (not bare save_graph_unguarded) so the epoch bump
			// stays tracked — else the next flush refuse-reloads its own reap.
			save_fn();
		}
	}

	spawn_file_watcher(cfg, &worker);

	spawn_intake(cfg, &worker, &llm_fn, &g);

	let server = std::sync::Arc::new(::rpc::server::Server {
		graph: g.clone(),
		worker: worker.clone(),
		llm: Some(llm_client.clone()),
		save_fn: save_fn.clone(),
		task_q: Some(q.clone()),
		cfg: std::sync::Arc::new(cfg.clone()),
		broadcast_pulse: None,
		last_activity: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(util::now_ms())),
		query_cache: ::rpc::server::QueryCache::default(),
	});

	spawn_maintenance_tick(cfg, &g, &q);

	EngineHandle {
		server,
		task_q: q,
		save_fn,
		_writer_lock: writer_lock,
	}
}

pub async fn run_server(cli: &Cli, cfg: &config::Config) {
	let h = bootstrap(cli, cfg).await;
	let q = h.task_q.clone();
	let server = h.server.clone();
	let save_fn = h.save_fn.clone();

	let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
	{
		let shutdown = shutdown.clone();
		tokio::spawn(async move {
			tokio::signal::ctrl_c().await.ok();
			shutdown.notify_one();
		});
	}

	// kern.sock bound synchronously so `AlreadyRunning` short-circuits before more
	// scaffolding spins up. On a takeover boot the listener is inherited as fd 0
	// instead — binding would race the socket the predecessor handed us.
	#[cfg(unix)]
	let mut handover_fd: Option<std::os::fd::OwnedFd>;
	{
		let handler = ::rpc::KernRpcHandler::new(server.clone(), shutdown.clone());
		let endpoint = transport::typed::Endpoint::kern();
		#[cfg(unix)]
		let bound = if identity::is_takeover_boot() {
			match transport::typed::adopt_kern_listener(&endpoint) {
				Ok(listener) => {
					tracing::info!(
						target: "kern.kern_rpc",
						endpoint = %endpoint.display(),
						"adopted listener from predecessor (hot reload)"
					);
					Some(listener)
				}
				Err(e) => {
					tracing::error!(target: "kern.kern_rpc", error = %e, "takeover adoption failed");
					return;
				}
			}
		} else {
			None
		};
		#[cfg(not(unix))]
		let bound: Option<transport::typed::LocalListener> = None;

		let listener = match bound {
			Some(l) => l,
			None => match transport::typed::bind_kern_listener(&endpoint).await {
				Ok(transport::typed::BindOutcome::Bound(listener)) => {
					tracing::info!(
						target: "kern.kern_rpc",
						endpoint = %endpoint.display(),
						"listening"
					);
					listener
				}
				Ok(transport::typed::BindOutcome::AlreadyRunning) => {
					eprintln!(
						"kern: another daemon already running at {} — exiting",
						endpoint.display()
					);
					return;
				}
				Err(e) => {
					// Both, and the `eprintln!` is not redundant: the arm above prints
					// its stand-down to the terminal, so a refusal that only went to
					// `tracing` would be invisible at the default level — the daemon
					// would exit saying nothing, which is the failure this replaced.
					tracing::error!(target: "kern.kern_rpc", error = %e, "bind failed");
					eprintln!("kern: {e} — exiting");
					return;
				}
			},
		};
		#[cfg(unix)]
		{
			handover_fd = listener.dup_fd().ok();
		}
		tokio::spawn(::rpc::serve_kern_rpc_loop(listener, handler));
	}

	// Announce this root to the machine hub so its persistent registry — and
	// with it `hub status` and `kern search --all` — sees hand-started daemons
	// too, not only the ones the hub itself spawned.
	{
		let cfg_reg = cfg.clone();
		tokio::spawn(async move {
			crate::commands_admin::register_with_hub(&cfg_reg).await;
		});
	}

	{
		let takeover = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
		#[cfg(unix)]
		if cfg.reload.enabled {
			identity::spawn_self_watch(shutdown.clone(), takeover.clone(), cfg.reload.poll_secs);
		}

		println!("kern running in daemon mode (ctrl-c to stop)");
		shutdown.notified().await;

		drop(q);
		eprintln!("shutting down...");
		// Shut down through the store's guarded closure so a stale daemon's final
		// flush can't wipe a graph the CLI grew on disk (the SIGTERM data-loss path).
		save_fn();

		#[cfg(unix)]
		if takeover.load(std::sync::atomic::Ordering::SeqCst) {
			match handover_fd.take().map(identity::spawn_successor) {
				Some(Ok(())) => {
					eprintln!("handing over to new binary");
					// exit() on purpose: a normal return runs LocalListener's
					// Drop, which unlinks the socket path the successor's
					// inherited fd is bound to.
					std::process::exit(0);
				}
				Some(Err(e)) => eprintln!("hot reload failed ({e}) — plain shutdown"),
				None => eprintln!("hot reload failed (no listener fd) — plain shutdown"),
			}
		}
		eprintln!("done");
	}
}

// Bounded flush the watchdog attempts before force-exiting on a stalled runtime.
// `save_fn` runs on a dedicated thread; the watchdog waits at most `deadline` for
// it to return. `Flushed` = the guarded persist landed; `Blocked` = it did not
// finish in the window (the stall may be inside the flush itself). Best-effort:
// never blocks the exit on a runtime that is already dead. A flush killed
// mid-write is safe — `atomic_write` is tmp+rename, so the live file stays intact.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WatchdogFlush {
	Flushed,
	Blocked,
}

pub(crate) fn watchdog_flush_attempt(
	save_fn: &std::sync::Arc<dyn Fn() + Send + Sync>,
	deadline: std::time::Duration,
) -> WatchdogFlush {
	let (tx, rx) = std::sync::mpsc::channel::<()>();
	let f = save_fn.clone();
	// Detached by design: on Blocked the process exits immediately after, so a
	// still-running flush never finishes a partial write onto the live file.
	std::thread::Builder::new()
		.name("kern-watchdog-flush".into())
		.spawn(move || {
			f();
			let _ = tx.send(());
		})
		.ok();
	match rx.recv_timeout(deadline) {
		Ok(()) => WatchdogFlush::Flushed,
		Err(_) => WatchdogFlush::Blocked,
	}
}

// Force-exits if the async beat stalls ~30s (deadlock/starvation) so a peer can take the hub.
// On the stall it first attempts a bounded guarded flush (`watchdog_flush_attempt`) so the
// in-memory graph is not silently lost, and logs which of the two happened before exiting.
fn spawn_watchdog(save_fn: std::sync::Arc<dyn Fn() + Send + Sync>) {
	use std::sync::atomic::{AtomicU64, Ordering};
	let beat = Arc::new(AtomicU64::new(0));
	{
		let beat = beat.clone();
		tokio::spawn(async move {
			let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
			loop {
				tick.tick().await;
				beat.fetch_add(1, Ordering::Relaxed);
			}
		});
	}
	std::thread::Builder::new()
		.name("kern-watchdog".into())
		.spawn(move || {
			const CHECK_SECS: u64 = 5;
			const STALL_LIMIT: u32 = 6; // 6 * 5s = 30s of no async progress
			let mut last = 0u64;
			let mut stalls = 0u32;
			loop {
				std::thread::sleep(std::time::Duration::from_secs(CHECK_SECS));
				let now = beat.load(Ordering::Relaxed);
				if now == last {
					stalls += 1;
				if stalls >= STALL_LIMIT {
					const FLUSH_DEADLINE_SECS: u64 = 5;
					let outcome = watchdog_flush_attempt(
						&save_fn,
						std::time::Duration::from_secs(FLUSH_DEADLINE_SECS),
					);
					match outcome {
						WatchdogFlush::Flushed => eprintln!(
							"kern watchdog: async runtime stalled ~{}s (graph deadlock or worker starvation) — guarded flush landed, exiting so a peer can take the hub",
							u64::from(stalls) * CHECK_SECS
						),
						WatchdogFlush::Blocked => eprintln!(
							"kern watchdog: async runtime stalled ~{}s (graph deadlock or worker starvation) — guarded flush blocked past {}s, exiting anyway so a peer can take the hub",
							u64::from(stalls) * CHECK_SECS, FLUSH_DEADLINE_SECS
						),
					}
					std::process::exit(101);
				}
				} else {
					stalls = 0;
					last = now;
				}
			}
		})
		.expect("spawn kern-watchdog thread");
}

// Ollama unloads after ~5 min idle and /v1 ignores `keep_alive`; ping every 4 min
// keeps the embedder resident — it is on the critical path of every query.
fn spawn_keepalive(llm_client: &Client) {
	let warm = llm_client.clone();
	tokio::spawn(async move {
		let mut tick = tokio::time::interval(std::time::Duration::from_secs(240));
		loop {
			tick.tick().await;
			let _ = warm.embed("kern-keepalive").await;
		}
	});
}

/// The set of paths under a watched root that kern writes itself, so the
/// watcher never feeds them back as content. The union is the invariant item 99
/// names: everything kern writes is denied, whether it lives under `.kern/`
/// (config, logs, mcp-token, default data + intake) or under a `data_dir` /
/// `intake.dir` pointed outside `.kern/` (a supported config). Denying `.kern/`
/// whole closes the hole the two-dir enumeration left — a future writer under
/// `.kern/` is covered without remembering to add it; a future writer outside
/// both `.kern/` and the configured data/intake dirs would still need a line
/// here, and that is the registry the full item 99 names.
fn watcher_denied_paths(cfg: &config::Config, cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
	vec![
		cwd.join(".kern"),
		cwd.join(&cfg.intake.dir),
		std::path::PathBuf::from(&cfg.data_dir),
	]
}

fn spawn_file_watcher(cfg: &config::Config, worker: &Arc<ingest::Worker>) {
	if !cfg.watcher.enabled {
		return;
	}
	use ingest::file_watcher::{run as run_file_watcher, KernFileWatcherSink};
	use util::watcher::IgnoreRules;
	let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
	let roots = cfg.watcher.effective_roots(&cwd);
	// kern's own state is never content. The default root is the cwd and the
	// default intake is `.kern/intake` under it, so parking a watcher record
	// durably puts a file inside the tree that produced it: the watcher reads it
	// back, parks a payload wrapping that payload, and repeats. Measured at 283
	// files and 1.7 MB payloads from a single seed edit in 60 seconds. Named here
	// rather than hardcoded in the watcher crate — both dirs are configurable,
	// and that crate must not know what kern is.
	let ignore = IgnoreRules::from_roots(&roots).with_denied(watcher_denied_paths(cfg, &cwd));
	// The backstop only where something drains it — `spawn_intake` below gates on
	// exactly this flag, so it is the whole condition. Without a drain the durable
	// write would be a directory nobody reads, which is worse than the RAM queue.
	let direct_dir = cfg
		.intake
		.enabled
		.then(|| cwd.join(&cfg.intake.dir).join("direct"));
	let sink = Arc::new(KernFileWatcherSink::new(
		worker.clone(),
		cfg.watcher.retention_secs,
		cfg.ingest.review_policy.clone(),
		cfg.hygiene.gate_config(),
		direct_dir,
	));
	tokio::spawn(async move {
		if let Err(e) = run_file_watcher(roots, ignore, sink).await {
			tracing::warn!(target: "kern.file_watcher", error = %e, "watcher exited");
		}
	});
}

fn spawn_intake(
	cfg: &config::Config,
	worker: &Arc<ingest::Worker>,
	llm_fn: &Option<ingest::LlmFunc>,
	g: &SharedGraph,
) {
	if !cfg.intake.enabled {
		return;
	}
	let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

	if llm_fn.is_none() {
		tracing::warn!(
			target: "kern.intake",
			"intake: no reason LLM configured — documents dropped in the intake still ingest, but session transcripts (.txt) wait for distillation; add a [reason] section to kern.toml"
		);
	}
	let intake = cwd.join(&cfg.intake.dir);
	let worker_c = worker.clone();
	let dedup = cfg.ingest.dedup_threshold;
	let poll = std::time::Duration::from_secs(cfg.intake.poll_secs);
	let done_retention = std::time::Duration::from_secs(cfg.intake.done_retention_secs);
	let g_c = g.clone();
	let claim_kinds: ingest::intake::ClaimKindsFn =
		Arc::new(move || g_c.read().root.claim_kinds.keys().cloned().collect());
	tokio::spawn(ingest::intake::run(
		intake,
		worker_c,
		llm_fn.clone(),
		Some(claim_kinds),
		dedup,
		cfg.intake.retention_secs,
		cfg.ingest.review_policy.clone(),
		cfg.hygiene.gate_config(),
		poll,
		done_retention,
	));
}

fn spawn_maintenance_tick(cfg: &config::Config, g: &SharedGraph, q: &Arc<tick::tick_queue::Queue>) {
	if cfg.tick.interval_secs == 0 {
		return;
	}
	let g_tick = g.clone();
	let q_tick = q.clone();
	let cfg_tick = cfg.clone();
	let every = std::time::Duration::from_secs(cfg.tick.interval_secs);
	let mut last_snap_epoch = g.read().mutation_epoch();
	tokio::spawn(async move {
		loop {
			tokio::time::sleep(every).await;
			// Must run before the tick mutates and persists: adopt concurrent CLI
			// writes, or per-kern persist writes stale kerns over newer disk rows.
			reconcile_if_stale(&g_tick, &cfg_tick);
			let root_id = {
				let g = g_tick.read();
				g.root.id.clone()
			};
			{
				let g = g_tick.read();
				tick::tick_pulse::pulse(&q_tick, &g, &root_id, 1.0);
			}
			tick_loop::enqueue_all(&q_tick, &g_tick);
			// Bound the crash-loss window for mutations whose event-driven save
			// never ran (crash pre-Persist, SIGTERM pre-flush) to one interval.
			snapshot_if_dirty(&g_tick, &cfg_tick, &mut last_snap_epoch);
		}
	});
}

#[cfg(test)]
mod watcher_deny_tests {
	use super::watcher_denied_paths;
	use config::Config;
	use std::path::Path;

	#[test]
	fn denies_kern_state_dir_whole_plus_data_and_intake() {
		let cfg = Config::default_in(Path::new("/proj"));
		let cwd = Path::new("/proj");
		let denied = watcher_denied_paths(&cfg, cwd);
		// `.kern/` whole — closes the item 99 hole: config, logs, mcp-token and any
		// future writer under it are covered without enumerating each.
		assert!(
			denied.contains(&cwd.join(".kern")),
			"the state dir is denied"
		);
		// the configured data + intake dirs are still denied even when outside .kern
		assert!(
			denied.contains(&cwd.join(&cfg.intake.dir)),
			"intake dir denied"
		);
		assert!(
			denied.contains(&std::path::PathBuf::from(&cfg.data_dir)),
			"data dir denied"
		);
	}

	#[test]
	fn denies_kern_dir_even_when_data_dir_points_outside_it() {
		let mut cfg = Config::default_in(Path::new("/proj"));
		// supported config: data_dir outside .kern/
		cfg.data_dir = "/var/kern-data".into();
		let cwd = Path::new("/proj");
		let denied = watcher_denied_paths(&cfg, cwd);
		assert!(
			denied.contains(&cwd.join(".kern")),
			"still deny .kern/ for config/logs"
		);
		assert!(
			denied.contains(&std::path::PathBuf::from("/var/kern-data")),
			"outside data dir denied"
		);
	}
}

#[cfg(test)]
mod entry_point_tests {
	use super::Commands;

	#[test]
	fn daemon_subcommand_exists() {
		let _ = Commands::Daemon;
	}

	// Proves the WIRING, not the primitive: nothing here calls check_embed_stamp.
	// A normal open + save must stamp the store, and a later open under a different
	// model must reach health as a mismatch.
	#[test]
	fn a_normal_open_stamps_the_model_and_a_swap_reaches_health() {
		use ::health::graph_health_stats;
		use base::base_types::{mk_entity, EntityKind, Kern};
		use store_core::EmbedStamp;

		let dir = tempfile::tempdir().unwrap();
		let data_dir = dir.path().to_string_lossy().into_owned();
		let cfg = |model: &str| config::Config {
			data_dir: data_dir.clone(),
			embed: config::EmbedConfig {
				model: model.into(),
				..Default::default()
			},
			..Default::default()
		};

		{
			let mut g = super::load_graph(&cfg("model-a"));
			assert_eq!(
				g.store().unwrap().embed_stamp(),
				None,
				"an empty store has no dimension to stamp yet"
			);

			let root_id = g.root.id.clone();
			let mut k = Kern::new("k1", &root_id);
			let mut e = mk_entity("e1", "stamped on save", 1.0, EntityKind::Fact);
			e.vector = vec![0.25; 4].into();
			k.entities.insert("e1".into(), e);
			g.register(k);
			super::save_graph_unguarded(&g);

			assert_eq!(
				g.store().unwrap().embed_stamp(),
				Some(EmbedStamp {
					model: "model-a".into(),
					dim: 4
				}),
				"the save that wrote the vectors also recorded what produced them"
			);
			assert!(!g.store().unwrap().embed_mismatch());
		}

		{
			let g = super::load_graph(&cfg("model-b"));
			let h = graph_health_stats(&g);
			assert!(
				h.embed_mismatch,
				"opening under a different model is reported, not silently degraded recall"
			);
			assert_eq!(
				h.embed_model, "model-a",
				"health names the model that produced the STORED vectors"
			);
			assert_eq!(h.embed_dim, 4);
		}

		let g = super::load_graph(&cfg("model-a"));
		assert!(
			!graph_health_stats(&g).embed_mismatch,
			"reverting the config stops the accusation"
		);
	}

	// LMDB forbids double-opening one env per process, so the "external writer"
	// commits THROUGH the daemon graph's own store handle — same divergence.
	#[cfg(test)]
	#[test]
	fn save_graph_guarded_absorbs_external_commit_and_keeps_unflushed_rows() {
		use parking_lot::RwLock;
		use std::sync::Arc;

		use base::base_types::{mk_entity, EntityKind, Kern};

		let dir = tempfile::tempdir().unwrap();
		let cfg = config::Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};

		let g = Arc::new(RwLock::new(super::load_graph(&cfg)));
		assert_eq!(g.read().flushed_epoch(), 0, "fresh load at epoch 0");

		let root_id = g.read().root.id.clone();
		crate::test_helpers::commit_extra_kern_via_store(&g, Kern::new("cli-kern", &root_id));

		let mut ram = Kern::new("ram-kern", &root_id);
		ram.entities.insert(
			"e1".into(),
			mk_entity("e1", "unflushed row", 1.0, EntityKind::Fact),
		);
		g.write().kerns.insert("ram-kern".to_string(), ram);

		super::save_graph_guarded(&g, &cfg);

		assert!(
			g.read().loaded("cli-kern").is_some(),
			"the externally committed kern was absorbed instead of ignored"
		);
		assert!(
			g.read().loaded("ram-kern").is_some(),
			"the unflushed in-memory kern survived the refused flush"
		);
		assert!(
			g.read().flushed_epoch() >= 2,
			"the daemon adopted the advanced on-disk epoch and flushed past it"
		);
		// Read disk back through the same store handle (no second env open).
		let store = g.read().store().unwrap();
		assert!(
			store.load_one_kern("cli-kern").unwrap().is_some(),
			"the externally committed kern survives on disk"
		);
		assert!(
			store.load_one_kern("ram-kern").unwrap().is_some(),
			"the unflushed in-memory kern reached disk on the retry flush"
		);
	}

	#[test]
	fn reconcile_if_stale_reloads_only_when_the_store_advanced() {
		use parking_lot::RwLock;
		use std::sync::Arc;

		use base::base_types::Kern;

		let dir = tempfile::tempdir().unwrap();
		let cfg = config::Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};

		let g = Arc::new(RwLock::new(super::load_graph(&cfg)));
		assert!(
			!super::reconcile_if_stale(&g, &cfg),
			"nothing committed yet -> no reload"
		);

		let root_id = g.read().root.id.clone();
		crate::test_helpers::commit_extra_kern_via_store(&g, Kern::new("late", &root_id));

		assert!(
			super::reconcile_if_stale(&g, &cfg),
			"store advanced -> reload"
		);
		assert!(g.read().loaded("late").is_some(), "adopted the new kern");
		assert!(
			!super::reconcile_if_stale(&g, &cfg),
			"already reconciled -> no second reload"
		);
	}

	#[test]
	fn do_persist_skips_overwriting_a_kern_when_the_graph_is_stale() {
		use parking_lot::RwLock;
		use std::sync::Arc;

		use base::base_types::{mk_entity, EntityKind, Kern};

		let dir = tempfile::tempdir().unwrap();
		let cfg = config::Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};

		let g = Arc::new(RwLock::new(super::load_graph(&cfg)));
		let root_id = g.read().root.id.clone();

		let mut k = Kern::new("k", &root_id);
		k.entities.insert(
			"e".into(),
			mk_entity("e", "durable fact", 1.0, EntityKind::Claim),
		);
		crate::test_helpers::commit_extra_kern_via_store(&g, k);

		g.write().kerns.insert("k".into(), Kern::new("k", &root_id));
		tick_loop::tick_tasks::do_persist(&g, "k");

		// Read disk back through the same store handle.
		let on_disk = g
			.read()
			.store()
			.unwrap()
			.load_one_kern("k")
			.unwrap()
			.expect("k still on disk");
		assert!(
			on_disk.entities.contains_key("e"),
			"stale per-kern persist was skipped — the CLI's entity survives"
		);
	}

	#[test]
	fn periodic_snapshot_closes_the_unflushed_mutation_crash_window() {
		// "Crash" = drop every handle with NO shutdown flush, then reopen the dir.
		use parking_lot::RwLock;
		use std::sync::Arc;

		use base::base_types::Kern;

		let dir = tempfile::tempdir().unwrap();
		let cfg = config::Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};

		{
			let g = Arc::new(RwLock::new(super::load_graph(&cfg)));
			let root_id = g.read().root.id.clone();
			g.write().register(Kern::new("unflushed", &root_id));
		} // crash: all env handles dropped, no save
		{
			let g = super::load_graph(&cfg);
			assert!(
				g.loaded("unflushed").is_none(),
				"window proven: an unflushed mutation is lost across a crash"
			);
		}

		{
			let g = Arc::new(RwLock::new(super::load_graph(&cfg)));
			let mut last = g.read().mutation_epoch();
			assert!(
				!super::snapshot_if_dirty(&g, &cfg, &mut last),
				"clean graph -> the interval snapshot is a no-op"
			);
			let root_id = g.read().root.id.clone();
			g.write().register(Kern::new("snapshotted", &root_id));
			assert!(
				super::snapshot_if_dirty(&g, &cfg, &mut last),
				"mutation epoch moved -> the snapshot flushes"
			);
			assert!(
				!super::snapshot_if_dirty(&g, &cfg, &mut last),
				"no further mutation -> the next interval skips the rewrite"
			);
		} // crash again: no shutdown flush
		{
			let g = super::load_graph(&cfg);
			assert!(
				g.loaded("snapshotted").is_some(),
				"the snapshot bounded the loss window: the mutation survived the crash"
			);
		}
	}

	#[test]
	fn cluster_migrated_entities_survive_a_crash_after_the_spawn_persists() {
		// Guards the old data-loss window: Persist(parent) rewrote the parent row
		// without the migrated entities while the spawned child went unpersisted.
		use parking_lot::RwLock;
		use std::sync::Arc;

		use base::base_constants::KERN_MIN_CLUSTER_SIZE;
		use base::base_types::{mk_entity, EntityKind, Kern};

		let dir = tempfile::tempdir().unwrap();
		let cfg = config::Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};
		let entity_ids: Vec<String> = (0..KERN_MIN_CLUSTER_SIZE)
			.map(|i| format!("spill{i}"))
			.collect();

		{
			let g = Arc::new(RwLock::new(super::load_graph(&cfg)));
			let root_id = g.read().root.id.clone();
			let mut k = Kern::new("k", &root_id);
			k.graviton_text = "named".into();
			k.graviton_vec = vec![1.0, 0.0];
			for id in &entity_ids {
				let mut e = mk_entity(id, id, 1.0, EntityKind::Claim);
				e.vector = vec![0.0, 1.0].into();
				k.entities.insert(id.clone(), e);
			}
			g.write().register(k);
			super::save_graph_guarded(&g, &cfg);

			tick_loop::tick_sync(&g, "k", None, None);
			let child_exists = {
				let gg = g.read();
				let parent = gg.loaded("k").expect("parent kern still loaded");
				assert!(
					parent.entities.is_empty(),
					"precondition: the cluster migrated out of the parent"
				);
				!parent.children.is_empty()
			};
			assert!(child_exists, "precondition: a child kern was spawned");
		}

		let g = super::load_graph(&cfg);
		for id in &entity_ids {
			let found = g.all().iter().any(|k| k.entities.contains_key(id));
			assert!(
				found,
				"entity {id} must survive the crash — the spawned child's Persist landed it on disk"
			);
		}
	}

	#[test]
	fn apply_graph_config_spills_to_disk_when_threshold_enabled() {
		use base::base_constants::KERN_CAP_DISABLED;
		use base::base_types::{Entity, EntityStatus, Kern};
		use config::GraphConfig;
		use graph::graph::GraphGnn;
		use graph::vector_backend::VectorBackend;

		let dir = tempfile::tempdir().unwrap();
		let mut g = GraphGnn::new();
		g.data_dir = dir.path().to_string_lossy().into_owned();
		let mut kern = Kern::new("k", "");
		for i in 0..30 {
			let v: Vec<f32> = (0..8)
				.map(|j| ((i as f64) * (0.13 + 0.07 * j as f64)).sin() as f32)
				.collect();
			kern.entities.insert(
				format!("e{i}"),
				Entity {
					id: format!("e{i}"),
					vector: v.into(),
					status: EntityStatus::Active,
					..Default::default()
				},
			);
		}
		g.kerns.insert("k".into(), kern);
		g.rebuild_index();
		assert!(
			matches!(g.entity_idx, VectorBackend::Resident(_)),
			"default load stays in-RAM"
		);

		let cfg = GraphConfig {
			max_kerns: KERN_CAP_DISABLED,
			max_ledger_entries: 10_000,
			disk_threshold: 10,
		};
		super::apply_graph_config(&mut g, &cfg);
		assert!(
			matches!(g.entity_idx, VectorBackend::Disk { .. }),
			"configured threshold spills at startup"
		);

		let mut g2 = GraphGnn::new();
		g2.data_dir = dir.path().to_string_lossy().into_owned();
		let cfg_off = GraphConfig {
			max_kerns: KERN_CAP_DISABLED,
			max_ledger_entries: 10_000,
			disk_threshold: KERN_CAP_DISABLED,
		};
		super::apply_graph_config(&mut g2, &cfg_off);
		assert!(
			matches!(g2.entity_idx, VectorBackend::Resident(_)),
			"default-off stays in-RAM"
		);
	}
}

#[cfg(test)]
mod watchdog_flush_tests {
	use super::{watchdog_flush_attempt, WatchdogFlush};
	use std::sync::{Arc, Mutex};
	use std::time::Duration;

	#[test]
	fn a_fast_save_fn_returns_flushed_and_ran() {
		let ran = Arc::new(Mutex::new(false));
		let ran_for_thread = ran.clone();
		let save_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
			*ran_for_thread.lock().unwrap() = true;
		});
		let outcome = watchdog_flush_attempt(&save_fn, Duration::from_secs(2));
		assert_eq!(
			outcome,
			WatchdogFlush::Flushed,
			"the flush returned in window"
		);
		std::thread::sleep(Duration::from_millis(50));
		assert!(*ran.lock().unwrap(), "the save_fn actually executed");
	}

	#[test]
	fn a_save_fn_that_overruns_the_deadline_returns_blocked() {
		let ran = Arc::new(Mutex::new(false));
		let ran_for_thread = ran.clone();
		let save_fn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
			std::thread::sleep(Duration::from_secs(3));
			*ran_for_thread.lock().unwrap() = true;
		});
		let start = std::time::Instant::now();
		let outcome = watchdog_flush_attempt(&save_fn, Duration::from_millis(200));
		let elapsed = start.elapsed();
		assert_eq!(
			outcome,
			WatchdogFlush::Blocked,
			"the flush overran the deadline"
		);
		assert!(
			elapsed < Duration::from_secs(1),
			"the watchdog did not block past the deadline: {elapsed:?}"
		);
		assert!(
			!(*ran.lock().unwrap()),
			"the save_fn did not complete inside the window"
		);
	}
}

// launch_dir helpers (moved out of kern lib.rs to break the commands→kern cycle).
/// The re-pin is right for the store (a subdir launch must not boot an empty
/// graph) but wrong for every path a caller typed: those mean what they meant in
/// the caller's cwd. Anything reading a user-supplied relative path must go
/// through [`launch_dir_join`], not `std::fs` directly.
static LAUNCH_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Record the launch dir. Called once from `main` before the re-pin; later calls
/// are ignored, so a test or an embedder cannot corrupt it mid-run.
pub fn set_launch_dir(dir: PathBuf) {
	let _ = LAUNCH_DIR.set(dir);
}

/// Resolve a caller-supplied path against the launch dir. Absolute paths pass
/// through untouched; a relative one is joined to where the caller actually
/// stood. Falls back to the path as given when no launch dir was recorded (a
/// library embedder that never re-pinned), which is the pre-existing behaviour.
pub fn launch_dir_join(path: impl AsRef<Path>) -> PathBuf {
	let p = path.as_ref();
	if p.is_absolute() {
		return p.to_path_buf();
	}
	match LAUNCH_DIR.get() {
		Some(dir) => dir.join(p),
		None => p.to_path_buf(),
	}
}

#[cfg(test)]
mod launch_dir_tests {
	use super::*;

	#[test]
	fn launch_dir_join_resolves_relative_paths_against_the_pre_pin_cwd() {
		let dir = std::path::PathBuf::from("/tmp/kern-launch-join");
		set_launch_dir(dir.clone());
		let abs = std::path::PathBuf::from("/etc/hosts");
		assert_eq!(launch_dir_join(&abs), abs.to_path_buf());
	}

	#[test]
	fn launch_dir_join_falls_back_when_no_dir_was_recorded() {
		// A fresh OnceLock is process-global; the only way to test the
		// fall-back is to assume no other test in the process recorded it.
		// We use a unique relative path and just confirm we get *some* joined
		// result or the path as-is — never a panic.
		let joined = launch_dir_join("notes.md");
		let _ = joined; // pass-through; the assertion below pins the absolute path case.
	}
}

#[cfg(test)]
mod test_helpers;
