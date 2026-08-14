//! Operator subcommands: health, gc, compact, compress, gravitons, claim
//! kinds, unnamed-kern triage, peers, hub control, and store registration —
//! administration of the daemon and its graph, not recall or ingest.

use graph::graph_ops::graviton_rows;

use transport::kern_rpc::AuthReq;
use transport::typed::Endpoint;

use util::short_id;

use crate::commands_route::{route_to, Routed};
use crate::{
	load_graph, save_graph_unguarded, with_graph, ClaimKindAction, Client, GravitonAction,
	UnnamedAction,
};

pub(crate) fn cmd_compress(src: &str, mode_str: &str, out: Option<&str>) {
	let Some(mode) = math::quant::QuantizationMode::parse(mode_str) else {
		eprintln!("compress: unknown mode '{mode_str}' (expected: none | int8)");
		return;
	};
	let mode_label = mode.as_str();
	let out_dir = out
		.map(|s| s.to_string())
		.unwrap_or_else(|| format!("{src}.{mode_label}"));
	if std::path::Path::new(&out_dir).exists() {
		eprintln!("compress: output path '{out_dir}' already exists; refusing to overwrite");
		return;
	}
	match graph::persist::compress_dir(src, &out_dir, mode) {
		Ok(()) => {
			let bpd = mode.bytes_per_dim();
			println!(
				"compressed {src} -> {out_dir}  mode={} (~{:.1} bytes/dim)",
				mode.as_str(),
				bpd,
			);
		}
		Err(e) => eprintln!("compress: {e}"),
	}
}

pub(crate) async fn cmd_health(cfg: &config::Config) {
	let g = load_graph(cfg);
	let h = ::health::graph_health_stats(&g);
	// Asked once, before anything prints: the degradation lines below need it too,
	// not just the tick lines.
	let d = daemon_health(cfg).await;

	println!("data_dir:    {}", g.data_dir);
	if h.gravitons.is_empty() {
		println!("gravitons:     (none)");
	} else {
		println!("gravitons:     {}", h.gravitons.join(", "));
	}
	let kerns_cap = if h.max_kerns == base::base_constants::KERN_CAP_DISABLED {
		"off".to_string()
	} else {
		h.max_kerns.to_string()
	};
	println!(
		"kerns:       {} (cap {}, largest {} entities, gini {:.2})",
		h.kerns, kerns_cap, h.largest_kern_entities, h.gini_kern_sizes
	);
	println!("thoughts:    {} (unnamed: {})", h.entities, h.unnamed);
	println!("reasons:     {}", h.reasons);
	println!("claim kinds: {}", g.root.claim_kinds.len());
	println!(
		"embed:       {} (dim {}){}",
		if h.embed_model.is_empty() {
			"(unstamped)"
		} else {
			&h.embed_model
		},
		h.embed_dim,
		if h.embed_mismatch {
			"  MISMATCH: the index was built with a different model"
		} else {
			""
		},
	);
	for line in degradation_lines(&h, d.as_ref()) {
		println!("{line}");
	}
	for line in tick_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in ingest_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in convergence_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in heat_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in retrieval_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in source_trust_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in dedup_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in kern_cap_health_lines(d.as_ref()) {
		println!("{line}");
	}
	for line in llm_health_lines(d.as_ref()) {
		println!("{line}");
	}

	for k in g.all() {
		let label = if k.graviton_text.is_empty() {
			"[unnamed]"
		} else {
			&k.graviton_text
		};
		println!(
			"  kern:{}  thoughts:{}  reasons:{}",
			label,
			k.entities.len(),
			k.reasons.len(),
		);
	}
}

// The tick queue lives in the daemon; an offline CLI has no view of it. One
// attempt, no retry: `kern health` must not stall when nothing is serving.
async fn daemon_health(cfg: &config::Config) -> Option<transport::kern_rpc::HealthRes> {
	use transport::kern_rpc::KernRpcClient;
	use transport::typed::{Endpoint, JsonEnvelopeCodec};

	let client = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		&Endpoint::kern(),
		&::rpc::caller_of(cfg),
		1,
		std::time::Duration::ZERO,
	)
	.await
	.ok()?;
	client.health().await.ok().filter(|h| h.ok)
}

// The store-and-fail-open half of the surface. Every number here is scoped to
// the process that reads it — seven `AtomicU64` statics plus a `Store` field
// `Store::open` zeroes — and nothing on the `kern health` path searches, scores,
// ticks, ingests or merges, so this process's copies can only be zero. A serving
// daemon's are the only true ones, so prefer them (ROADMAP item 100); the local
// values still stand when nothing is serving. Pure over its inputs, like
// `tick_health_lines`: reading the statics here would make any test of it depend
// on what else ran in the same process.
fn degradation_lines(
	h: &::health::HealthStats,
	d: Option<&transport::kern_rpc::HealthRes>,
) -> Vec<String> {
	let [cold_evicted, query_dim_rejected, below_floor_deliveries, clock_skew_skips, ingest_dropped_chunks, remote_cap_dropped, unspilled_drops, ingest_queue_refused] =
		match d {
			Some(d) => [
				d.cold_evicted,
				d.query_dim_rejected,
				d.below_floor_deliveries,
				d.clock_skew_skips,
				d.ingest_dropped_chunks,
				d.remote_cap_dropped,
				d.unspilled_drops,
				d.ingest_queue_refused,
			],
			None => [
				h.cold_evicted,
				h.query_dim_rejected,
				h.below_floor_deliveries,
				h.clock_skew_skips,
				h.ingest_dropped_chunks,
				h.remote_cap_dropped,
				h.unspilled_drops,
				h.ingest_queue_refused,
			],
		};
	let mut lines = vec![format!("evicted:     {cold_evicted} cold rows dropped")];
	// Fail-open is the policy; invisible fail-open is the defect (ROADMAP item 7).
	// Print the line only when something actually degraded, so a healthy kern stays
	// quiet and a nonzero count is impossible to scroll past.
	let degraded = query_dim_rejected
		+ below_floor_deliveries
		+ clock_skew_skips
		+ ingest_dropped_chunks
		+ remote_cap_dropped
		+ unspilled_drops
		+ ingest_queue_refused;
	if degraded > 0 {
		lines.push(format!(
			"degraded:    {} off-model queries dropped, {} below-floor deliveries, {} clock-skewed entities GC could not age, {} chunks lost to embedding, {} remote ids refused at the cap, {} dropped with nowhere to spill, {} ingest jobs refused at the queue bound",
			query_dim_rejected,
			below_floor_deliveries,
			clock_skew_skips,
			ingest_dropped_chunks,
			remote_cap_dropped,
			unspilled_drops,
			ingest_queue_refused
		));
	}
	lines
}

fn tick_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	let Some(h) = h else {
		return vec!["tick:        (no daemon serving this directory)".to_string()];
	};
	let mut lines = vec![
		format!(
			"tick:        queue {} | done {} | avg {} ms",
			h.queue_depth, h.tasks_done, h.task_avg_ms
		),
		format!(
			"degraded:    {} panics | {} failures | {} refused GNN trainings | {} supersede chains past hop budget",
			h.task_panics, h.task_failures, h.gnn_train_refused, h.supersede_chain_depth_exceeded
		),
	];
	if !h.last_task_panic.is_empty() {
		lines.push(format!("  last panic:   {}", h.last_task_panic));
	}
	if !h.last_task_failure.is_empty() {
		lines.push(format!("  last failure: {}", h.last_task_failure));
	}
	lines
}

// The ingest RAM queue's fill. Daemon-sourced like the tick lines, and for the
// same reason: the CLI's own worker is idle by construction, so a local read is
// structurally zero. No daemon, no line — a gauge nobody holds is not 0.
fn ingest_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	match h {
		Some(h) => vec![format!("ingest:      queue {}", h.ingest_queue_depth)],
		None => Vec::new(),
	}
}

// The Gini-over-access convergence metric (ROADMAP item 62). Daemon-sourced
// like the ingest queue line: the CLI's own graph is a fresh open with no query
// history, so its access distribution is structurally uniform (gini 0.0) and a
// local read carries no signal. No daemon, no line.
fn convergence_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	match h {
		Some(h) => vec![format!("convergence: gini {:.2}", h.gini_access)],
		None => Vec::new(),
	}
}

// Active heat retention half-life (ROADMAP item 62 `kern://health` surfacing).
// Daemon-sourced like the convergence line: the CLI's own config is irrelevant
// — the daemon's running preset is what the operator asked about, and a fresh
// CLI opens on defaults that carry no signal. No daemon, no line. 0 (old
// daemon / unset) prints `0s` unconditionally, matching the `convergence:` line
// that prints `gini 0.00` when a daemon answers.
fn heat_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	match h {
		Some(h) => vec![
			// Active preset name (ROADMAP item 87 measurement half). The frame
			// the heat/recency/retrieval lines interpret. Empty = old daemon.
			format!("preset:      {}", h.preset),
			format!("heat:        half-life {}s", h.heat_half_life_secs),
			// QBST recency half-life — the 24h ranking-freshness signal, the
			// second of item 55's two freshness signals. Same daemon-sourced
			// rule as the heat line above.
			format!("recency:     half-life {}s", h.qbst_recency_half_life_secs),
		],
		None => Vec::new(),
	}
}

// Active RRF config + mode blends (ROADMAP item 66 measurement half).
// Daemon-sourced like the heat/recency lines: the CLI's own config is
// irrelevant — the daemon's running preset is what the operator asked about.
// No daemon, no block. A zeroed block (old daemon) prints zeroes, matching
// the `convergence:`/`heat:` lines that print `0.00`/`0s` when a daemon
// answers.
fn retrieval_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	match h {
		Some(h) => vec![
			format!(
				"retrieval:   rrf_k {}, global {}",
				h.retrieval.rrf_k, h.retrieval.rrf_global_weight
			),
			format!(
				"  content {{ content {}, reason {}, edge {} }}",
				h.retrieval.weights_content.content,
				h.retrieval.weights_content.reason,
				h.retrieval.weights_content.edge,
			),
			format!(
				"  reason  {{ content {}, reason {}, edge {} }}",
				h.retrieval.weights_reason.content,
				h.retrieval.weights_reason.reason,
				h.retrieval.weights_reason.edge,
			),
			format!(
				"  hybrid  {{ content {}, reason {}, edge {} }}",
				h.retrieval.weights_hybrid.content,
				h.retrieval.weights_hybrid.reason,
				h.retrieval.weights_hybrid.edge,
			),
			format!(
				"  seed_k {}, mmr {}, lexical {}, pagerank {}",
				h.retrieval.seed_k,
				h.retrieval.mmr_enabled,
				h.retrieval.lexical_enabled,
				h.retrieval.pagerank_enabled,
			),
		],
		None => Vec::new(),
	}
}

// Active source-trust map (ROADMAP item 20 measurement half). Daemon-sourced
// like the heat/recency/retrieval lines: the CLI's own config is irrelevant —
// the daemon's running `source_trust` is what the operator asked about. No
// daemon, no line. An empty map (unconfigured kern) prints `source_trust:
// (none)`, matching the preset/heat lines that print a zeroed value when a
// daemon answers.
fn source_trust_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	let Some(h) = h else {
		return Vec::new();
	};
	if h.source_trust.is_empty() {
		return vec!["source_trust: (none)".to_string()];
	}
	let pairs: Vec<String> = h
		.source_trust
		.iter()
		.map(|(scheme, w)| format!("{scheme}={w}"))
		.collect();
	vec![format!("source_trust: {}", pairs.join(", "))]
}

// Active ingest dedup config (ROADMAP item 48 measurement half): the global
// `dedup_threshold` plus the per-kind `dedup_threshold_by_kind` array (item 48
// beside, shipped 2026-07-23). `None` falls back to the global. Daemon-sourced
// like the heat/recency/retrieval/source_trust lines: the CLI's own config is
// irrelevant (item 100). No daemon, no line.
fn dedup_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	let Some(h) = h else {
		return Vec::new();
	};
	// The 5 EntityKind labels in `as u8` order (Fact .. Conclusion).
	let kinds = ["fact", "claim", "document", "question", "conclusion"];
	let overrides: Vec<String> = h
		.ingest_dedup_threshold_by_kind
		.iter()
		.enumerate()
		.filter_map(|(i, v)| v.map(|w| format!("{}={}", kinds[i], w)))
		.collect();
	if overrides.is_empty() {
		vec![format!("dedup: {}", h.ingest_dedup_threshold)]
	} else {
		vec![format!(
			"dedup: {}, kind {}",
			h.ingest_dedup_threshold,
			overrides.join(", ")
		)]
	}
}

// The resident-kern cap approach warn (ROADMAP item 83). Daemon-sourced like
// the convergence line: the CLI's own graph is a fresh open with one kern, so
// its resident count is structurally small and a local read carries no signal.
// No daemon, no warn. `KERN_CAP_DISABLED` (u64::MAX) and 0 (old daemon / unset)
// both read as "cap off" — an opt-out or an absent field is not a bound.
fn kern_cap_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	let Some(h) = h else {
		return Vec::new();
	};
	let cap = h.max_kerns;
	if cap == 0 || cap == u64::MAX {
		return Vec::new();
	}
	let resident = h.kerns;
	if (resident as f64) >= base::base_constants::KERN_CAP_APPROACH_FRAC * (cap as f64) {
		vec![format!("kerns near cap: {}/{}", resident, cap)]
	} else {
		Vec::new()
	}
}

// The completion leg's failures (ROADMAP item 30). Daemon-sourced for the same
// reason as the tick lines: the counter is a process static and nothing on the
// `kern health` path completes anything, so a local read could only be zero.
// Quiet at zero, loud otherwise — the count is what separates a dead endpoint
// from a model too weak to answer, and the string is what separates a timeout
// from a refusal from an empty body.
fn llm_health_lines(h: Option<&transport::kern_rpc::HealthRes>) -> Vec<String> {
	let Some(h) = h.filter(|h| h.llm_complete_failed > 0) else {
		return Vec::new();
	};
	let mut lines = vec![format!(
		"llm:         {} failed completions",
		h.llm_complete_failed
	)];
	if !h.last_llm_complete_failure.is_empty() {
		lines.push(format!(
			"  last llm failure: {}",
			h.last_llm_complete_failure
		));
	}
	lines
}

// Daemon must be stopped: a live daemon would race and re-persist the bloated graph.
pub(crate) fn cmd_gc(cfg: &config::Config) {
	let _lock = match store::lock::acquire(&cfg.data_dir, "gc") {
		Ok(l) => l,
		Err(e) => {
			eprintln!("gc: {e}");
			eprintln!("  stop it first — a live daemon re-persists the graph this reaped from");
			return;
		}
	};
	let mut g = load_graph(cfg);
	let (before, reaped, after) = g.gc_empty_kerns_counted();
	save_graph_unguarded(&g);
	println!("gc: reaped {reaped} empty kerns ({before} -> {after})");

	// Drop the graph FIRST to release its env handle: compact_dir closes its own
	// env deterministically — a lazy drop on Windows leaves data.mdb mmap'd.
	drop(g);
	match store_core::compact_dir(&cfg.data_dir) {
		Ok((old, new)) => println!(
			"gc: compacted data.mdb {} -> {} ({:.0}% reclaimed)",
			human_bytes(old),
			human_bytes(new),
			if old > new && old > 0 {
				(old - new) as f64 * 100.0 / old as f64
			} else {
				0.0
			},
		),
		Err(e) => eprintln!("gc: compaction failed: {e}"),
	}
}

// Daemon must be stopped: compaction swaps data.mdb underneath any open env.
pub(crate) fn cmd_compact(cfg: &config::Config) {
	let _lock = match store::lock::acquire(&cfg.data_dir, "compact") {
		Ok(l) => l,
		Err(e) => {
			eprintln!("compact: {e}");
			eprintln!("  stop it first — compaction renames data.mdb under any open environment");
			return;
		}
	};
	match store_core::compact_dir(&cfg.data_dir) {
		Ok((old, new)) => println!(
			"compact: data.mdb {} -> {} ({:.0}% reclaimed)",
			human_bytes(old),
			human_bytes(new),
			if old > 0 {
				(old - new) as f64 * 100.0 / old as f64
			} else {
				0.0
			},
		),
		Err(e) => eprintln!("compact: failed: {e}"),
	}
}

fn human_bytes(n: u64) -> String {
	const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
	let mut v = n as f64;
	let mut i = 0;
	while v >= 1024.0 && i < U.len() - 1 {
		v /= 1024.0;
		i += 1;
	}
	if i == 0 {
		format!("{n} B")
	} else {
		format!("{v:.1} {}", U[i])
	}
}

fn print_graviton_added(name: &str, mass: f64) {
	println!("graviton added: {name} (mass {mass})");
}

fn print_graviton_removed(name: &str) {
	println!("graviton removed: {name}");
}

pub(crate) async fn cmd_graviton(cfg: &config::Config, action: GravitonAction) {
	graviton_at(cfg, &Endpoint::kern(), &::rpc::caller_of(cfg), action).await
}

// Routed first for the same reason as forget: `with_graph` writes the whole kern
// map back unguarded, so a local graviton edit beside a serving daemon drops
// everything that daemon has committed since this process loaded.
async fn graviton_at(
	cfg: &config::Config,
	endpoint: &Endpoint,
	auth: &AuthReq,
	action: GravitonAction,
) {
	match action {
		GravitonAction::Add {
			name,
			text,
			mass,
			embed,
		} => {
			let mass = mass.unwrap_or(1.0);
			// Routed before the embed: the daemon owns the vector it stores, and
			// embedding here would be a second call to the same model for nothing.
			match route_to(
				endpoint,
				auth,
				"graviton",
				serde_json::json!({"action": "add", "name": &name, "text": &text, "mass": mass}),
			)
			.await
			{
				Routed::Done(_) => return print_graviton_added(&name, mass),
				Routed::Refused(e) => return eprintln!("{e}"),
				Routed::NoDaemon => {}
			}
			let (url, model) = embed.resolve(cfg);
			let llm_client = Client::new_embed_only(url, model, &cfg.embed.key);
			// Multi-line seed = example statements, embedded separately and
			// mean-pooled (see accept::seed_examples for the measurement).
			let mut vecs = Vec::new();
			for ex in graph::accept::seed_examples(&text) {
				match llm_client.embed(&ex).await {
					Ok(v) => vecs.push(v),
					Err(e) => {
						eprintln!("embed: {e}");
						return;
					}
				}
			}
			let Some(vec) = graph::accept::mean_pool(&vecs) else {
				eprintln!("embed: empty or mismatched embeddings");
				return;
			};
			with_graph(cfg, |g| {
				graph::accept::add_graviton_with_mass(g, &name, vec, mass)
			});
			print_graviton_added(&name, mass);
		}
		GravitonAction::List => {
			let g = load_graph(cfg);
			println!("gravitons:");
			for r in graviton_rows(&g) {
				println!(
					"  {}  mass:{}  thoughts:{}  reasons:{}",
					r.name, r.mass, r.thoughts, r.reasons,
				);
			}
		}
		GravitonAction::Remove { name } => {
			match route_to(
				endpoint,
				auth,
				"graviton",
				serde_json::json!({"action": "remove", "name": &name}),
			)
			.await
			{
				Routed::Done(_) => return print_graviton_removed(&name),
				Routed::Refused(e) => return eprintln!("{e}"),
				Routed::NoDaemon => {}
			}
			let removed = with_graph(cfg, |g| graph::accept::remove_graviton(g, &name));
			if removed {
				print_graviton_removed(&name);
			} else {
				eprintln!("graviton not found: {name}");
			}
		}
	}
}
fn print_claim_kind_added(name: &str) {
	println!("claim kind added: {name}");
}

fn print_claim_kind_removed(name: &str) {
	println!("claim kind removed: {name}");
}

pub(crate) async fn cmd_claim_kind(cfg: &config::Config, action: ClaimKindAction) {
	claim_kind_at(cfg, &Endpoint::kern(), &::rpc::caller_of(cfg), action).await
}

async fn claim_kind_at(
	cfg: &config::Config,
	endpoint: &Endpoint,
	auth: &AuthReq,
	action: ClaimKindAction,
) {
	match action {
		ClaimKindAction::Add {
			name,
			description,
			parent,
		} => {
			match route_to(
				endpoint,
				auth,
				"claim_kind",
				serde_json::json!({"action": "add", "name": &name, "description": &description, "parent": parent.as_deref().unwrap_or("")}),
			)
			.await
			{
				Routed::Done(_) => return print_claim_kind_added(&name),
				Routed::Refused(e) => return eprintln!("{e}"),
				Routed::NoDaemon => {}
			}
			let mut refused: Option<String> = None;
			with_graph(cfg, |g| {
				if let Err(e) = g.root.add_claim_kind(
					&name,
					&description,
					parent.as_deref(),
					&ingest::distill::DEFAULT_KINDS,
				) {
					refused = Some(e);
				}
			});
			match refused {
				Some(e) => eprintln!("{e}"),
				None => print_claim_kind_added(&name),
			}
		}
		ClaimKindAction::Rm { name } => {
			match route_to(
				endpoint,
				auth,
				"claim_kind",
				serde_json::json!({"action": "rm", "name": &name}),
			)
			.await
			{
				Routed::Done(_) => return print_claim_kind_removed(&name),
				Routed::Refused(e) => return eprintln!("{e}"),
				Routed::NoDaemon => {}
			}
			with_graph(cfg, |g| {
				g.root.rm_claim_kind(&name);
			});
			print_claim_kind_removed(&name);
		}
	}
}

pub(crate) fn cmd_peers(cfg: &config::Config) {
	print!("{}", peers_summary(cfg));
}

fn peers_summary(cfg: &config::Config) -> String {
	let g = &cfg.gossip;
	let mut out = String::new();
	if !g.enabled {
		out.push_str("gossip:  disabled\n");
		out.push_str("  enable with [gossip] enabled = true in kern.toml\n");
		return out;
	}
	out.push_str("gossip:     enabled\n");
	out.push_str(&format!("addr:       {}\n", g.addr));
	out.push_str(&format!(
		"discovery:  {} (udp :{})\n",
		if g.discovery { "on" } else { "off" },
		g.discovery_port
	));
	if g.peers.is_empty() {
		out.push_str("peers:      (none configured)\n");
	} else {
		out.push_str(&format!("peers ({}):\n", g.peers.len()));
		for p in &g.peers {
			out.push_str(&format!("  {p}\n"));
		}
	}
	out.push_str("  (runtime-discovered peers visible in daemon logs)\n");
	out
}

pub(crate) fn cmd_register(cfg: &config::Config, path: &str) {
	// The loaded graph is bound to the SOURCE store, so write into a freshly
	// opened destination store — save_graph_unguarded would write back to the source.
	match graph::persist::load_dir(path) {
		Ok(g) => match store_core::Store::open(&cfg.data_dir) {
			Ok(dest) => {
				let _ = graph::persist::save_graph_into(&dest, &g);
				println!("registered {path}");
			}
			Err(e) => eprintln!("register: {e}"),
		},
		Err(e) => eprintln!("load: {e}"),
	}
}

pub(crate) async fn cmd_unnamed(cfg: &config::Config, action: UnnamedAction) {
	match action {
		UnnamedAction::List => {
			let g = load_graph(cfg);
			let mut found = false;
			for k in g.all() {
				if k.is_unnamed() {
					println!(
						"unnamed  id:{}  thoughts:{}",
						short_id(&k.id),
						k.entities.len()
					);
					found = true;
				}
			}
			if !found {
				println!("no unnamed kerns");
			}
		}
		UnnamedAction::Promote {
			id,
			name,
			text,
			mass,
			embed,
		} => {
			let mass = mass.unwrap_or(1.0);
			let (url, model) = embed.resolve(cfg);
			let llm_client = Client::new_embed_only(url, model, &cfg.embed.key);
			let mut vecs = Vec::new();
			for ex in graph::accept::seed_examples(&text) {
				match llm_client.embed(&ex).await {
					Ok(v) => vecs.push(v),
					Err(e) => {
						eprintln!("embed: {e}");
						return;
					}
				}
			}
			let Some(vec) = graph::accept::mean_pool(&vecs) else {
				eprintln!("embed: empty or mismatched embeddings");
				return;
			};
			// Resolve a short id to the full kern id the way `kern unnamed` prints it.
			let full = {
				let g = load_graph(cfg);
				g.all()
					.into_iter()
					.map(|k| k.id.clone())
					.find(|kid| short_id(kid) == id || kid == &id)
			};
			let Some(full) = full else {
				eprintln!("no unnamed kern matching id {id}");
				return;
			};
			with_graph(cfg, |g| {
				if let Err(e) = graph::accept::promote_unnamed(g, &full, &name, vec.clone(), mass) {
					eprintln!("{e}");
				}
			});
			println!("promoted unnamed {id} -> graviton {name} (mass {mass})");
		}
	}
}

fn default_root() -> String {
	let cwd = std::env::current_dir().unwrap_or_default();
	config::Config::resolve_root(&cwd).display().to_string()
}

pub(crate) async fn cmd_hub(action: Option<crate::HubAction>, idle_unload_secs: u64) {
	use transport::hub_rpc::{HubRpcClient, ResolveReq, UnloadReq};
	use transport::typed::JsonEnvelopeCodec;

	match action {
		None => ::hub::run_hub(idle_unload_secs).await,
		Some(crate::HubAction::Resolve { root }) => {
			let root = root.unwrap_or_else(default_root);
			let client = match HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
				Ok(c) => c,
				Err(e) => {
					eprintln!("hub: not running ({e})");
					return;
				}
			};
			match client.resolve(ResolveReq { root: root.clone() }).await {
				Ok(res) if res.ok => println!(
					"{}  {}",
					if res.spawned { "spawned" } else { "running" },
					res.endpoint
				),
				Ok(res) => eprintln!("resolve {root}: {}", res.err),
				Err(e) => eprintln!("hub resolve: {e}"),
			}
		}
		Some(crate::HubAction::Status) => {
			let client = match HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
				Ok(c) => c,
				Err(e) => {
					eprintln!("hub: not running ({e})");
					return;
				}
			};
			match client.status().await {
				Ok(res) => {
					if res.nodes.is_empty() {
						println!("hub: running, no nodes");
					}
					for n in res.nodes {
						println!(
							"{}  pid:{}  {}  {}",
							if n.alive { "up  " } else { "dead" },
							n.pid,
							n.root,
							n.endpoint
						);
					}
				}
				Err(e) => eprintln!("hub status: {e}"),
			}
		}
		Some(crate::HubAction::Unload { root }) => {
			let root = root.unwrap_or_else(default_root);
			let client = match HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
				Ok(c) => c,
				Err(e) => {
					eprintln!("hub: not running ({e})");
					return;
				}
			};
			match client.unload(UnloadReq { root: root.clone() }).await {
				Ok(res) if res.ok && res.existed => println!("unloaded {root}"),
				Ok(res) if res.ok => println!("no node for {root}"),
				Ok(res) => eprintln!("unload {root}: {}", res.err),
				Err(e) => eprintln!("hub unload: {e}"),
			}
		}
		Some(crate::HubAction::Merge { src, dst }) => cmd_hub_merge(&src, &dst).await,
		Some(crate::HubAction::Stop) => match HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
			Ok(client) => match client.stop().await {
				Ok(_) => println!("hub stopped (nodes stay up)"),
				Err(e) => eprintln!("hub stop: {e}"),
			},
			Err(e) => eprintln!("hub: not running ({e})"),
		},
	}
}

// Offline CRDT union: src's rows and topology join dst's store; src is never
// written. Both daemons must be down — the store is single-writer and a live
// daemon's flush would clobber the merge.
async fn cmd_hub_merge(src: &str, dst: &str) {
	use transport::hub_rpc::{HubRpcClient, UnloadReq};
	use transport::typed::JsonEnvelopeCodec;

	let canon = |s: &str| -> Option<std::path::PathBuf> {
		let p = std::path::Path::new(s).canonicalize().ok()?;
		Some(config::Config::resolve_root(&p))
	};
	let Some(src_root) = canon(src) else {
		eprintln!("merge: src {src} does not exist");
		return;
	};
	let Some(dst_root) = canon(dst) else {
		eprintln!("merge: dst {dst} does not exist");
		return;
	};
	if src_root == dst_root {
		eprintln!(
			"merge: src and dst are the same root {}",
			src_root.display()
		);
		return;
	}
	if !src_root.join(".kern").is_dir() {
		eprintln!("merge: src {} has no .kern store", src_root.display());
		return;
	}

	if let Ok(client) = HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
		for root in [&src_root, &dst_root] {
			let _ = client
				.unload(UnloadReq {
					root: root.display().to_string(),
				})
				.await;
		}
	}
	for root in [&src_root, &dst_root] {
		if ::hub::probe(root).await {
			eprintln!(
				"merge: a daemon still serves {} — stop it first",
				root.display()
			);
			return;
		}
	}

	// Fallback must stay pinned to the root: a bare `Config::default()` carries a
	// cwd-relative data_dir and would read (and write!) whatever store the
	// caller happens to stand in.
	let src_cfg = match config::Config::load(&src_root) {
		Ok(c) => c,
		Err(e) => {
			eprintln!("merge: src config error: {e}");
			return;
		}
	};
	let dst_cfg = match config::Config::load(&dst_root) {
		Ok(c) => c,
		Err(e) => {
			eprintln!("merge: dst config error: {e}");
			return;
		}
	};
	let src_g = load_graph(&src_cfg);
	let mut dst_g = load_graph(&dst_cfg);

	let src_h = ::health::graph_health_stats(&src_g);
	if src_h.entities == 0 {
		eprintln!("merge: src {} holds no entities", src_root.display());
		return;
	}
	let before = ::health::graph_health_stats(&dst_g);
	let changed = graph::merge::absorb_graph(&mut dst_g, src_g);
	save_graph_unguarded(&dst_g);
	let after = ::health::graph_health_stats(&dst_g);
	println!(
		"merged {} -> {}: {} rows joined, entities {} -> {}, kerns {} -> {} (src untouched)",
		src_root.display(),
		dst_root.display(),
		changed,
		before.entities,
		after.entities,
		before.kerns,
		after.kerns,
	);
}

use transport::kern_rpc::KernRpcClient;
use transport::typed::JsonEnvelopeCodec;

pub(crate) async fn cmd_status(cfg: &config::Config) {
	let kern_ep = Endpoint::kern();
	let hub_ep = Endpoint::hub();

	println!("data dir     {}", cfg.data_dir);
	println!("kern socket  {}", kern_ep.display());

	let caller = ::rpc::caller_of(cfg);
	let daemon = probe(&kern_ep, &caller).await;
	match &daemon {
		Some(h) => println!(
			"daemon       serving  ({} kerns, {} entities, idle {}s)",
			h.kerns,
			h.entities,
			h.idle_ms / 1000
		),
		None => println!("daemon       not serving this directory"),
	}

	match probe(&hub_ep, &caller).await {
		Some(_) => println!("hub          running   {}", hub_ep.display()),
		None => println!("hub          not running"),
	}

	// Read AFTER the probes: a daemon that answers but holds no lock is the
	// state worth seeing, and it is exactly what an older binary produces.
	match store::lock::holder(&cfg.data_dir) {
		Some(who) => {
			println!("writer lock  held by {who}");
			println!();
			println!("Offline admin commands (reembed, compact, gc) will refuse while it is held.");
		}
		None => {
			println!("writer lock  free");
			if daemon.is_some() {
				println!();
				println!(
					"A daemon is serving but holds no writer lock — it predates the lock, or could not \
					 take it. Offline admin commands will NOT be refused; stop it before running one."
				);
			}
		}
	}

	// F5 (RECALL_PLAN): surface the OTHER stores on this machine so a near-empty
	// store is not mistaken for "the" memory. Cheap stat walk only — no store is
	// opened, so a daemon sitting on one is never disturbed. Walks the directory
	// ancestors of the current data dir up to $HOME, scanning each level's
	// children for `<x>/.pi/kern` and `<x>/.kern` data dirs.
	let current = std::path::PathBuf::from(&cfg.data_dir);
	let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
	let mut others: Vec<std::path::PathBuf> = Vec::new();
	let mut probe_dir = current.parent();
	while let Some(dir) = probe_dir {
		if home.as_deref().is_some_and(|h| dir == h) {
			break;
		}
		if let Ok(entries) = std::fs::read_dir(dir) {
			for e in entries.flatten() {
				let p = e.path();
				for rel in [".pi/kern", ".kern"] {
					let cand = p.join(rel);
					if cand.join("data").is_dir() && cand != current && !others.contains(&cand) {
						others.push(cand);
					}
				}
			}
		}
		probe_dir = dir.parent();
	}
	if !others.is_empty() {
		println!();
		println!(
			"other kern stores on this machine (this session reads only {}):",
			current.display()
		);
		for o in others {
			println!("  {}", o.display());
		}
		println!("  -> set KERN_DIR to one of these to pin the store");
	}
}

// One attempt, no retry: status must answer instantly when nothing is there.
// A caller the daemon refuses reads as "not serving" here, the same as one that
// found nothing — this line describes reachability, and an unreachable daemon is
// unreachable either way. `route` is where the distinction has teeth.
async fn probe(
	ep: &Endpoint,
	auth: &transport::kern_rpc::AuthReq,
) -> Option<transport::kern_rpc::HealthRes> {
	KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		ep,
		auth,
		1,
		std::time::Duration::ZERO,
	)
	.await
	.ok()?
	.health()
	.await
	.ok()
	.filter(|h| h.ok)
}

#[cfg(test)]
#[path = "tests/commands_admin_test.rs"]
mod commands_admin_tests;
