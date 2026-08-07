//! Operator subcommands: health, gc, compact, compress, gravitons, claim
//! kinds, unnamed-kern triage, peers, hub control, and store registration —
//! administration of the daemon and its graph, not recall or ingest.

use transport::kern_rpc::AuthReq;
use transport::typed::Endpoint;

use util::short_id;

use crate::commands::{
	load_graph, save_graph_unguarded, with_graph, ClaimKindAction, Client, GravitonAction,
	UnnamedAction,
};
use crate::commands_route::{route_to, Routed};

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
		&crate::rpc::caller_of(cfg),
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
	match store::base_store::compact_dir(&cfg.data_dir) {
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
	match store::base_store::compact_dir(&cfg.data_dir) {
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
	graviton_at(cfg, &Endpoint::kern(), &crate::rpc::caller_of(cfg), action).await
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

pub(crate) struct GravitonRow {
	pub(crate) name: String,
	pub(crate) mass: f64,
	pub(crate) thoughts: usize,
	pub(crate) reasons: usize,
}

pub(crate) fn graviton_rows(g: &graph::graph::GraphGnn) -> Vec<GravitonRow> {
	graph::accept::root_graviton_ids(g)
		.iter()
		.filter_map(|cid| g.loaded(cid))
		.map(|c| GravitonRow {
			name: c.graviton_text.clone(),
			mass: c.mass,
			thoughts: c.entities.len(),
			reasons: c.reasons.len(),
		})
		.collect()
}

fn print_claim_kind_added(name: &str) {
	println!("claim kind added: {name}");
}

fn print_claim_kind_removed(name: &str) {
	println!("claim kind removed: {name}");
}

pub(crate) async fn cmd_claim_kind(cfg: &config::Config, action: ClaimKindAction) {
	claim_kind_at(cfg, &Endpoint::kern(), &crate::rpc::caller_of(cfg), action).await
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
		Ok(g) => match store::base_store::Store::open(&cfg.data_dir) {
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

pub(crate) async fn cmd_hub(action: Option<crate::commands::HubAction>, idle_unload_secs: u64) {
	use transport::hub_rpc::{HubRpcClient, ResolveReq, UnloadReq};
	use transport::typed::JsonEnvelopeCodec;

	match action {
		None => crate::hub::run_hub(idle_unload_secs).await,
		Some(crate::commands::HubAction::Resolve { root }) => {
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
		Some(crate::commands::HubAction::Status) => {
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
		Some(crate::commands::HubAction::Unload { root }) => {
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
		Some(crate::commands::HubAction::Merge { src, dst }) => cmd_hub_merge(&src, &dst).await,
		Some(crate::commands::HubAction::Stop) => {
			match HubRpcClient::<JsonEnvelopeCodec>::connect_hub().await {
				Ok(client) => match client.stop().await {
					Ok(_) => println!("hub stopped (nodes stay up)"),
					Err(e) => eprintln!("hub stop: {e}"),
				},
				Err(e) => eprintln!("hub: not running ({e})"),
			}
		}
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
		if crate::hub::probe(root).await {
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

	let caller = crate::rpc::caller_of(cfg);
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
mod degradation_lines_tests {
	use super::*;
	use ::health::HealthStats;
	use transport::kern_rpc::HealthRes;

	// What a CLI can actually see of the eight: it opened its own store and ran
	// no query, no tick and no ingest, so every one of them is zero.
	fn blind_cli() -> HealthStats {
		HealthStats::default()
	}

	#[test]
	fn a_serving_daemons_counts_win_over_this_processs_zeros() {
		let daemon = HealthRes {
			ok: true,
			cold_evicted: 3,
			ingest_dropped_chunks: 9,
			..Default::default()
		};
		let lines = degradation_lines(&blind_cli(), Some(&daemon));
		assert_eq!(lines[0], "evicted:     3 cold rows dropped");
		assert!(
			lines[1].contains("9 chunks lost to embedding"),
			"the daemon's drops reach the operator reading them: {lines:?}"
		);
	}

	#[test]
	fn a_local_count_is_not_printed_over_a_serving_daemons() {
		// The inverted case, which a merge of the two sources would pass: the
		// daemon is healthy and this process is not, and the daemon is what runs.
		let local = HealthStats {
			cold_evicted: 4,
			unspilled_drops: 6,
			..Default::default()
		};
		let daemon = HealthRes {
			ok: true,
			..Default::default()
		};
		let lines = degradation_lines(&local, Some(&daemon));
		assert_eq!(lines[0], "evicted:     0 cold rows dropped");
		assert_eq!(lines.len(), 1, "nothing degraded over there: {lines:?}");
	}

	#[test]
	fn with_nothing_serving_the_local_counts_still_stand() {
		let local = HealthStats {
			cold_evicted: 2,
			unspilled_drops: 5,
			..Default::default()
		};
		let lines = degradation_lines(&local, None);
		assert_eq!(lines[0], "evicted:     2 cold rows dropped");
		assert!(
			lines[1].contains("5 dropped with nowhere to spill"),
			"the offline path is unchanged: {lines:?}"
		);
	}

	#[test]
	fn a_healthy_kern_prints_no_degraded_line_from_either_source() {
		assert_eq!(degradation_lines(&blind_cli(), None).len(), 1);
		let healthy = HealthRes {
			ok: true,
			..Default::default()
		};
		assert_eq!(degradation_lines(&blind_cli(), Some(&healthy)).len(), 1);
	}

	#[test]
	fn kern_health_warns_when_resident_kerns_approach_cap() {
		// 116/128 >= 0.9*128 (115.2) -> warn line present.
		let near = HealthRes {
			ok: true,
			kerns: 116,
			max_kerns: 128,
			..Default::default()
		};
		let lines = kern_cap_health_lines(Some(&near));
		assert_eq!(lines, vec!["kerns near cap: 116/128"]);

		// 10/128 -> no warn.
		let fine = HealthRes {
			ok: true,
			kerns: 10,
			max_kerns: 128,
			..Default::default()
		};
		assert!(
			kern_cap_health_lines(Some(&fine)).is_empty(),
			"under the approach fraction -> no warn"
		);

		// KERN_CAP_DISABLED (u64::MAX) -> cap off, no warn.
		let uncapped = HealthRes {
			ok: true,
			kerns: 1000,
			max_kerns: u64::MAX,
			..Default::default()
		};
		assert!(
			kern_cap_health_lines(Some(&uncapped)).is_empty(),
			"uncapped -> no warn"
		);

		// 0 (old daemon / unset) -> no warn.
		let old = HealthRes {
			ok: true,
			kerns: 1000,
			max_kerns: 0,
			..Default::default()
		};
		assert!(
			kern_cap_health_lines(Some(&old)).is_empty(),
			"absent cap field -> no warn"
		);

		// No daemon -> no warn.
		assert!(kern_cap_health_lines(None).is_empty());
	}

	#[test]
	fn kern_health_prints_heat_half_life() {
		// 30d (relaxed preset) -> half-life 2592000s, line present.
		let relaxed = HealthRes {
			ok: true,
			heat_half_life_secs: 2592000,
			..Default::default()
		};
		let lines = heat_health_lines(Some(&relaxed));
		assert_eq!(
			lines,
			vec![
				"preset:      ",
				"heat:        half-life 2592000s",
				"recency:     half-life 0s",
			]
		);

		// 0 (old daemon / unset) -> prints 0s unconditionally, matching the
		// convergence: line that prints `gini 0.00` when a daemon answers.
		let old = HealthRes {
			ok: true,
			heat_half_life_secs: 0,
			..Default::default()
		};
		assert_eq!(
			heat_health_lines(Some(&old)),
			vec![
				"preset:      ",
				"heat:        half-life 0s",
				"recency:     half-life 0s",
			],
			"a daemon that answers carries the line even at 0"
		);

		// No daemon -> no line: the CLI's own config is irrelevant.
		assert!(
			heat_health_lines(None).is_empty(),
			"no daemon -> no heat line"
		);

		// 24h QBST recency half-life -> the recency line carries it (ROADMAP
		// item 55 measurement half). The heat line stays at its own value.
		let recency = HealthRes {
			ok: true,
			heat_half_life_secs: 2592000,
			qbst_recency_half_life_secs: 86400,
			..Default::default()
		};
		assert_eq!(
			heat_health_lines(Some(&recency)),
			vec![
				"preset:      ",
				"heat:        half-life 2592000s",
				"recency:     half-life 86400s",
			],
			"recency half-life surfaced daemon-sourced"
		);
	}

	#[test]
	fn kern_health_prints_source_trust() {
		// A configured source-trust map prints one line naming each scheme.
		let cfg = HealthRes {
			ok: true,
			source_trust: std::collections::BTreeMap::from([
				("file".to_string(), 0.8),
				("ticket".to_string(), 0.9),
			]),
			..Default::default()
		};
		assert_eq!(
			source_trust_health_lines(Some(&cfg)),
			vec!["source_trust: file=0.8, ticket=0.9"],
		);

		// An unconfigured kern (empty map) prints a (none) line — the
		// bit-identical default, not absent.
		let uncfg = HealthRes {
			ok: true,
			..Default::default()
		};
		assert_eq!(
			source_trust_health_lines(Some(&uncfg)),
			vec!["source_trust: (none)"],
			"empty map surfaces a (none) line, matching the zeroed-value rule"
		);

		// No daemon -> no line (item 100 rule: the CLI's own config is
		// irrelevant, the daemon's running map is what the operator asked
		// about).
		assert!(
			source_trust_health_lines(None).is_empty(),
			"no daemon -> no source_trust line"
		);
	}

	#[test]
	fn kern_health_prints_dedup_config() {
		// A configured per-kind override prints one line with the global + the
		// override (ROADMAP item 48 measurement half).
		let cfg = HealthRes {
			ok: true,
			ingest_dedup_threshold: 0.95,
			ingest_dedup_threshold_by_kind: [Some(0.99), None, None, None, None],
			..Default::default()
		};
		assert_eq!(
			dedup_health_lines(Some(&cfg)),
			vec!["dedup: 0.95, kind fact=0.99"],
		);

		// All-None -> global only, no kind suffix.
		let uncfg = HealthRes {
			ok: true,
			ingest_dedup_threshold: 0.95,
			..Default::default()
		};
		assert_eq!(
			dedup_health_lines(Some(&uncfg)),
			vec!["dedup: 0.95"],
			"all-None per-kind surfaces the global only"
		);

		// No daemon -> no line (item 100 rule).
		assert!(
			dedup_health_lines(None).is_empty(),
			"no daemon -> no dedup line"
		);
	}

	#[test]
	fn kern_health_prints_preset() {
		// A named preset -> the line carries it (ROADMAP item 87 measurement half).
		let tight = HealthRes {
			ok: true,
			preset: "tight".into(),
			..Default::default()
		};
		let lines = heat_health_lines(Some(&tight));
		assert!(
			lines.iter().any(|l| l == "preset:      tight"),
			"preset name surfaced: {lines:?}"
		);

		// Relaxed (the default) -> relaxed.
		let relaxed = HealthRes {
			ok: true,
			preset: "relaxed".into(),
			..Default::default()
		};
		assert!(
			heat_health_lines(Some(&relaxed))
				.iter()
				.any(|l| l == "preset:      relaxed"),
			"relaxed preset named"
		);

		// Empty (old daemon) -> empty name, line still present.
		let old = HealthRes {
			ok: true,
			preset: String::new(),
			..Default::default()
		};
		assert!(
			heat_health_lines(Some(&old))
				.iter()
				.any(|l| l == "preset:      "),
			"old daemon -> empty preset name"
		);

		// No daemon -> no line.
		assert!(
			!heat_health_lines(None)
				.iter()
				.any(|l| l.starts_with("preset:")),
			"no daemon -> no preset line"
		);
	}

	#[test]
	fn kern_health_prints_retrieval_config() {
		// Non-default retrieval block -> the four lines carry it daemon-sourced.
		let cfg = HealthRes {
			ok: true,
			retrieval: transport::kern_rpc::RetrievalHealth {
				rrf_k: 60.0,
				rrf_global_weight: 0.5,
				weights_content: transport::kern_rpc::ModeWeightsHealth {
					content: 0.7,
					reason: 0.2,
					edge: 0.1,
				},
				weights_reason: transport::kern_rpc::ModeWeightsHealth {
					content: 0.1,
					reason: 0.8,
					edge: 0.1,
				},
				weights_hybrid: transport::kern_rpc::ModeWeightsHealth {
					content: 0.5,
					reason: 0.3,
					edge: 0.2,
				},
				seed_k: 30,
				mmr_enabled: false,
				lexical_enabled: true,
				pagerank_enabled: true,
			},
			..Default::default()
		};
		let lines = retrieval_health_lines(Some(&cfg));
		assert_eq!(lines.len(), 5, "header + three mode-weight + one knob line");
		assert!(
			lines[0].contains("retrieval:") && lines[0].contains("60") && lines[0].contains("0.5"),
			"header carries rrf_k + global: {lines:?}"
		);
		assert!(
			lines[1].contains("content") && lines[1].contains("0.7"),
			"content weights line: {lines:?}"
		);
		assert!(
			lines[2].contains("reason") && lines[2].contains("0.8"),
			"reason weights line: {lines:?}"
		);
		assert!(
			lines[3].contains("hybrid") && lines[3].contains("0.2"),
			"hybrid weights line: {lines:?}"
		);
		assert!(
			lines[4].contains("seed_k 30")
				&& lines[4].contains("mmr false")
				&& lines[4].contains("lexical true")
				&& lines[4].contains("pagerank true"),
			"knob line carries the four: {lines:?}"
		);

		// Zeroed (old daemon / unset) -> five lines of zeroes, matching the
		// heat/recency lines that print `0s` when a daemon answers.
		let old = HealthRes {
			ok: true,
			..Default::default()
		};
		assert_eq!(
			retrieval_health_lines(Some(&old)).len(),
			5,
			"old daemon still prints the block at zero"
		);

		// No daemon -> no block: the CLI's own config is irrelevant.
		assert!(
			retrieval_health_lines(None).is_empty(),
			"no daemon -> no retrieval block"
		);
	}
}

#[cfg(test)]
mod peers_tests {
	use super::*;
	use config::Config;

	#[test]
	fn peers_summary_gossip_disabled() {
		let cfg = Config::default();
		let s = peers_summary(&cfg);
		assert!(s.contains("disabled"), "disabled state shown");
		assert!(s.contains("enabled = true"), "enable hint shown");
	}

	#[test]
	fn peers_summary_enabled_no_seed_peers() {
		let mut cfg = Config::default();
		cfg.gossip.enabled = true;
		let s = peers_summary(&cfg);
		assert!(s.contains("enabled"), "enabled state shown");
		assert!(s.contains("none configured"), "empty peer list shown");
	}

	#[test]
	fn peers_summary_enabled_with_seed_peers() {
		let mut cfg = Config::default();
		cfg.gossip.enabled = true;
		cfg.gossip.peers = vec!["192.168.1.10:7400".into(), "192.168.1.11:7400".into()];
		let s = peers_summary(&cfg);
		assert!(s.contains("192.168.1.10:7400"), "first peer listed");
		assert!(s.contains("192.168.1.11:7400"), "second peer listed");
		assert!(s.contains("peers (2)"), "count shown");
	}
}

#[cfg(test)]
mod cmd_tests {
	use super::*;
	use config::Config;

	fn temp_cfg() -> (tempfile::TempDir, Config) {
		let dir = tempfile::tempdir().expect("tempdir");
		let cfg = Config {
			data_dir: dir.path().to_string_lossy().into_owned(),
			..Default::default()
		};
		(dir, cfg)
	}

	#[cfg(unix)]
	#[tokio::test(flavor = "multi_thread")]
	async fn claim_kind_add_then_remove_persists_through_the_graph() {
		let (_dir, cfg) = temp_cfg();
		// An endpoint nothing ever bound: the NoDaemon fallback, pinned so the
		// test can never reach a daemon the developer happens to be running.
		let ep = crate::test_support::scratch_endpoint("claim-kind-local");
		// A custom key, not a default: default keys re-inject on every load, so Rm
		// would appear to fail on the next load.
		let key = "custom_test_kind";

		claim_kind_at(
			&cfg,
			&ep,
			&crate::test_support::test_caller(),
			ClaimKindAction::Add {
				name: key.into(),
				description: "a custom kind".into(),
				parent: None,
			},
		)
		.await;
		let g = load_graph(&cfg);
		assert_eq!(
			g.root.claim_kinds.get(key).map(String::as_str),
			Some("a custom kind"),
			"Add persists the claim kind onto the root",
		);

		claim_kind_at(
			&cfg,
			&ep,
			&crate::test_support::test_caller(),
			ClaimKindAction::Rm { name: key.into() },
		)
		.await;
		let g = load_graph(&cfg);
		assert!(
			!g.root.claim_kinds.contains_key(key),
			"Rm removes the custom claim kind"
		);
	}

	// The half of item 9 this closes: beside a serving daemon the command must
	// hand the write over, because the local path is `with_graph` — load, mutate,
	// `save_graph_unguarded` — which writes the whole kern map back with no epoch
	// check and drops every commit the daemon made since that load.
	#[cfg(unix)]
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_claim_kind_add_lands_in_the_daemon_and_never_touches_the_store() {
		let (_dir, cfg) = temp_cfg();
		let ep = crate::test_support::scratch_endpoint("claim-kind-routed");
		let srv = crate::test_support::mcp_server();
		let graph = srv.graph.clone();
		crate::test_support::serving(srv, &ep).await;

		claim_kind_at(
			&cfg,
			&ep,
			&crate::test_support::test_caller(),
			ClaimKindAction::Add {
				name: "custom_test_kind".into(),
				description: "a custom kind".into(),
				parent: None,
			},
		)
		.await;

		assert_eq!(
			graph
				.read()
				.root
				.claim_kinds
				.get("custom_test_kind")
				.map(String::as_str),
			Some("a custom kind"),
			"the serving daemon's own graph took the write"
		);
		assert!(
			!load_graph(&cfg)
				.root
				.claim_kinds
				.contains_key("custom_test_kind"),
			"the CLI's store was never written behind the daemon's back"
		);
	}

	#[cfg(unix)]
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_graviton_remove_lands_in_the_daemon_and_never_touches_the_store() {
		let (_dir, cfg) = temp_cfg();
		// The local store carries the same graviton, so a command that fell through
		// to `with_graph` would visibly delete it here.
		with_graph(&cfg, |g| {
			graph::accept::add_graviton_with_mass(g, "docs", vec![1.0, 0.0], 1.0)
		});

		let ep = crate::test_support::scratch_endpoint("graviton-routed");
		let srv = crate::test_support::mcp_server();
		let graph = srv.graph.clone();
		graph::accept::add_graviton_with_mass(&mut graph.write(), "docs", vec![1.0, 0.0], 1.0);
		crate::test_support::serving(srv, &ep).await;

		graviton_at(
			&cfg,
			&ep,
			&crate::test_support::test_caller(),
			GravitonAction::Remove {
				name: "docs".into(),
			},
		)
		.await;

		assert!(
			graviton_rows(&graph.read()).is_empty(),
			"the serving daemon's own graph lost the graviton"
		);
		assert_eq!(
			graviton_rows(&load_graph(&cfg))
				.iter()
				.map(|r| r.name.clone())
				.collect::<Vec<_>>(),
			vec!["docs".to_string()],
			"the CLI's store is untouched — the daemon owns the write"
		);
	}

	#[tokio::test]
	async fn cmd_health_runs_on_a_fresh_graph_without_panicking() {
		let (_dir, cfg) = temp_cfg();
		cmd_health(&cfg).await;
	}

	#[test]
	fn tick_health_lines_report_both_degradation_counters() {
		let offline = tick_health_lines(None);
		assert_eq!(offline.len(), 1, "no daemon -> no invented numbers");
		assert!(offline[0].contains("no daemon"), "{offline:?}");

		let live = tick_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			task_panics: 2,
			last_task_panic: "GnnPropagate[k]: boom".into(),
			task_failures: 3,
			last_task_failure: "GnnPropagate[k]: train epoch 0 forward".into(),
			..Default::default()
		}))
		.join("\n");
		assert!(live.contains("2 panics | 3 failures"), "{live}");
		assert!(
			live.contains("last panic:   GnnPropagate[k]: boom"),
			"{live}"
		);
		assert!(
			live.contains("last failure: GnnPropagate[k]: train epoch 0 forward"),
			"{live}"
		);
	}

	// This counter alone, every other one zero — which is the only state it is
	// ever seen in, since the trainer refusing has nothing to do with a task
	// panicking. A line gated on some other counter reports nothing here.
	#[test]
	fn a_refused_gnn_training_shows_with_no_other_counter_moving() {
		let lines = tick_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			gnn_train_refused: 4,
			..Default::default()
		}));
		assert_eq!(lines.len(), 2, "counts only, no fault lines: {lines:?}");
		assert!(lines[1].contains("4 refused GNN trainings"), "{lines:?}");
	}

	#[test]
	fn the_ingest_line_prints_the_daemons_depth_and_nothing_without_one() {
		assert!(
			ingest_health_lines(None).is_empty(),
			"no daemon -> no invented numbers"
		);
		let lines = ingest_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			ingest_queue_depth: 5,
			..Default::default()
		}));
		assert_eq!(lines, vec!["ingest:      queue 5".to_string()]);
	}

	// The three outcomes item 30 says were one empty string. Each drives the same
	// counter, so the count alone cannot tell them apart — the named last failure
	// is what does, and the test would pass on a count-only surface without it.
	#[test]
	fn the_llm_line_names_which_failure_it_counted() {
		assert!(
			llm_health_lines(None).is_empty(),
			"no daemon -> no invented numbers"
		);
		assert!(
			llm_health_lines(Some(&transport::kern_rpc::HealthRes {
				ok: true,
				..Default::default()
			}))
			.is_empty(),
			"a healthy completion leg stays quiet"
		);

		for reason in [
			"transient: HTTP error: operation timed out",
			"transient: HTTP error: tcp connect error",
			"permanent: empty completion response",
		] {
			let lines = llm_health_lines(Some(&transport::kern_rpc::HealthRes {
				ok: true,
				llm_complete_failed: 7,
				last_llm_complete_failure: reason.into(),
				..Default::default()
			}));
			assert_eq!(lines.len(), 2, "{lines:?}");
			assert!(lines[0].contains("7 failed completions"), "{lines:?}");
			assert!(lines[1].contains(reason), "{lines:?}");
		}
	}

	#[test]
	fn a_clean_daemon_prints_no_last_fault_lines() {
		let lines = tick_health_lines(Some(&transport::kern_rpc::HealthRes {
			ok: true,
			..Default::default()
		}));
		assert_eq!(
			lines.len(),
			2,
			"healthy tick reports counts only: {lines:?}"
		);
	}
}

#[cfg(test)]
mod hub_merge_tests {
	use base::base_types::{mk_entity, EntityKind, Kern};

	fn store_with_entity(root: &std::path::Path, eid: &str) {
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let cfg = config::Config::default_in(root);
		let mut g = graph::graph::GraphGnn::new();
		g.data_dir = cfg.data_dir.clone();
		std::fs::create_dir_all(&g.data_dir).unwrap();
		let mut k = Kern::new("k-hub-merge", g.root.id.clone());
		k.root_id = g.root.id.clone();
		k.graviton_text = "merge test".into();
		k.entities.insert(
			eid.to_string(),
			mk_entity(eid, "merged fact", 1.0, EntityKind::Fact),
		);
		g.register(k);
		// save_all silently no-ops without a store attached.
		let store = store::base_store::Store::open(&g.data_dir).unwrap();
		g.set_store(std::sync::Arc::new(store));
		graph::persist::save_all(&g).unwrap();
	}

	fn dst_entities(root: &std::path::Path) -> usize {
		let cfg = config::Config::default_in(root);
		let g = crate::commands::load_graph(&cfg);
		::health::graph_health_stats(&g).entities
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn merge_absorbs_src_entities_into_dst_and_leaves_src_alone() {
		let dir = tempfile::tempdir().unwrap();
		let src = dir.path().join("src");
		let dst = dir.path().join("dst");
		store_with_entity(&src, "e-src");
		store_with_entity(&dst, "e-dst");
		assert_eq!(dst_entities(&src), 1, "src store persisted before merge");

		super::cmd_hub_merge(&src.display().to_string(), &dst.display().to_string()).await;

		assert_eq!(
			dst_entities(&dst),
			2,
			"dst holds its own + the absorbed entity"
		);
		assert_eq!(dst_entities(&src), 1, "src is never written");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn merge_refuses_identical_roots_and_missing_src() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("only");
		store_with_entity(&root, "e-1");
		let r = root.display().to_string();

		// Same root: refused before any store is touched.
		super::cmd_hub_merge(&r, &r).await;
		assert_eq!(dst_entities(&root), 1, "self-merge is a refused no-op");

		// Missing src: refused.
		super::cmd_hub_merge("/nonexistent/kern-merge-src", &r).await;
		assert_eq!(dst_entities(&root), 1, "missing src leaves dst untouched");
	}
}
