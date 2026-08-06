use std::time::Duration;

// The balanced hybrid mix: ModeWeights::default() and RetrievalConfig.weights_hybrid.
pub const DEFAULT_WEIGHT_CONTENT: f64 = 0.5;
pub const DEFAULT_WEIGHT_REASON: f64 = 0.3;
pub const DEFAULT_WEIGHT_EDGE: f64 = 0.2;

pub const DEFAULT_SEED_K: usize = 5;

pub const QBST_ACCESS_WEIGHT: f64 = 0.02;
pub const QBST_RECENCY_WEIGHT: f64 = 0.05;
pub const QBST_RECENCY_HALF_LIFE: Duration = Duration::from_secs(24 * 60 * 60);
pub const QBST_CAP: f64 = 0.1;

pub const COLD_COMPACT_MIN_BYTES: u64 = 256 * 1024;

pub const COLD_MAX_ENTRIES: usize = 50_000;

// ef=1 is greedy single-path and misses true NN, letting duplicates through.
pub const DEDUP_EF: usize = 64;

pub const INGEST_DEDUP_THRESHOLD: f64 = 0.95;

pub const TICK_MAX_CLUSTER_SAMPLE: usize = 200;
pub const TICK_QUEUE_CAPACITY: usize = 512;
// 0 disables the driver.
pub const TICK_INTERVAL_SECS: u64 = 60;

// "no kern-eviction cap" sentinel. A finite cap is currently unsafe.
pub const KERN_CAP_DISABLED: usize = usize::MAX;

// Fraction of `max_loaded_kerns` at which `kern health` warns the operator the
// resident set is approaching the cap (ROADMAP item 83). A knob, not a policy.
pub const KERN_CAP_APPROACH_FRAC: f64 = 0.9;

// Measured 2026-07-21 against qwen3-embedding:0.6b on a 40-claim labelled
// corpus: intended claim-to-graviton cosine distances run 0.29-0.69 (median
// ~0.55 for description seeds, ~0.39 for mean-pooled example seeds), while
// off-topic claims stay >= 0.57. The old 0.15/0.35 put the acceptance midpoint
// at 0.25 — BELOW every real match, so routing never fired and everything fell
// to generic. Midpoint (inner+outer)/2 = 0.55 admits real matches and still
// rejects off-topic with margin. Stored per-kern: existing kerns keep their
// old radii until reseeded.
pub const KERN_INNER_RADIUS: f64 = 0.35;
pub const KERN_OUTER_RADIUS: f64 = 0.75;

pub const ACCEPT_FLOOR: f64 = 0.5;

// Empty graviton_vec, so similarity routing never matches it — fallback only.
pub const GENERIC_GRAVITON: &str = "generic";

// Two gravitons above this cosine are the SAME concept. Re-tune if the embed model changes.
pub const GRAVITON_DEDUP_THRESHOLD: f64 = 0.85;
// A single-paragraph graviton seed above this many chars is char-chunked into
// separate embeds the caller mean_pools — a long paragraph stops truncating past
// the embed context window with no signal. Default 4000 ≈ 1000 tokens, well under
// a 4–8k prompt budget and under any real seed today, so the path is a no-op until
// an operator lowers it (same default-off shape as DISTILL_CHUNK_TURNS / item 57).
pub const GRAVITON_SEED_CHAR_CHUNK: usize = 4000;
pub const KERN_COHESION_THRESHOLD: f64 = 0.60;
pub const KERN_MIN_CLUSTER_SIZE: usize = 10;
pub const KERN_NAMING_COHESION_THRESHOLD: f64 = 0.50;
pub const KERN_NAMING_MIN_CLUSTER_SIZE: usize = 5;

pub const PULSE_DECAY: f64 = 0.5;
pub const PULSE_THRESHOLD: f64 = 0.05;

pub const REFINE_TRAVERSAL_WEIGHT: f64 = 0.01;
pub const REFINE_BOOST_CAP: f64 = 0.1;
pub const TRAVERSAL_CREDIT_CAP: f64 = 0.5;
pub const TRAVERSAL_CREDIT_WEIGHT: f64 = 2.0;

pub const IMPORTANT_MIN_COSINE: f64 = 0.20;
pub const IMPORTANT_ACCESS_THRESHOLD: i32 = 3;

pub const FACT_SCORE_BOOST: f64 = 0.3;

pub const DEFAULT_CONFIDENCE: f64 = 0.5;
pub const MAX_AI_CONFIDENCE: f64 = 0.95;
pub const FACT_CONFIDENCE: f64 = 1.0;

// Ranking uses the lower confidence bound `conf_mean - K*sqrt(conf_variance)`
// (Wilson-style), not the mean alone — so a single-observation claim stops
// outranking a well-evidenced one at equal mean (ROADMAP item 65). One
// standard deviation; auto-tuning is item 66.
pub const CONFIDENCE_BOUND_K: f64 = 1.0;

pub const PROVENANCE_SCORE: f64 = 0.85;

pub const QUERY_MAX_CHAINS: usize = 5;

pub const DEGRADE_DECAY_BASE: f64 = 0.15;
pub const DEGRADE_DECAY_POW: f64 = 0.75;
pub const DEGRADE_MIN_THRESHOLD: f64 = 0.05;
/// Lower bound a `degrade`d edge score clamps at — a score is a non-negative
/// relevance weight, so repeated degrades never push it negative (item 59 floor).
pub const DEGRADE_FLOOR: f64 = 0.0;

/// Half-life in seconds for evidence decay — tick-based γ damping of
/// `conf_alpha`/`conf_beta` toward the Jeffreys prior (1,1). `0` = disabled
/// (the decay pass is a noop, bit-identical today); the operator opts in. The
/// policy (enable-by-default + the rate) is a separate decision (item 57).
pub const EVIDENCE_HALF_LIFE_SECS: u64 = 0;

/// Max turns per distill prompt — a turn is a blank-line-delimited block. A
/// conversation longer than this is chunked turn-batched so a long delta stops
/// truncating past the model context window with no signal (item 49 chunking
/// half). 48 turns keeps one distill call well under a 4–8k-token prompt; the
/// batch is a knob, not a product choice.
pub const DISTILL_CHUNK_TURNS: usize = 48;

/// Supersede-chain depth at which the `SUPERSEDE_CHAIN_DEPTH_EXCEEDED` counter
/// increments — the doc's "~5 hops on the same `external_id`" that would flip
/// kern to full versioning (item 58 trigger #1). A tunable const, not a
/// product choice; the counter measures, it does not act.
pub const SUPERSEDE_CHAIN_HOP_THRESHOLD: usize = 5;

pub const QUESTION_RESOLVE_THRESHOLD: f64 = 0.80;

pub const GOSSIP_FANOUT: usize = 3;
pub const GOSSIP_SEEN_SET_CAP: usize = 10_000;
pub const GOSSIP_SEEN_TTL: Duration = Duration::from_secs(60);
pub const GOSSIP_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
pub const GOSSIP_DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);
pub const GOSSIP_DISCOVERY_MULTICAST: &str = "239.77.75.68";
pub const GOSSIP_MAX_PEERS: usize = 50;
pub const GOSSIP_SEED_ADDR: &str = "seed.kern.dev:7946";
pub const GOSSIP_DIAL_TIMEOUT: Duration = Duration::from_secs(2);
pub const GOSSIP_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
// Reject a larger declared length before reading any body bytes — bounds per-connection memory.
pub const GOSSIP_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

// At the cap new remote ids are dropped; existing ids still CRDT-merge (known updates never lost).
pub const GOSSIP_REMOTE_KERN_ENTITY_CAP: usize = 50_000;
// Coarsely bounds a peer pinning a per-replica slot toward u64::MAX.
pub const GOSSIP_CRDT_DELTA_MAX: u64 = 1_000_000;

pub const LEDGER_THOUGHT_TTL: Duration = Duration::from_secs(72 * 60 * 60);
pub const LEDGER_ROUTING_TTL: Duration = Duration::from_secs(5 * 60);

// Residency, not forgetting: an unloaded kern is persisted first and reloads on next `get`.
pub const KERN_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
pub const KERN_IDLE_SWEEP_EVERY: Duration = Duration::from_secs(60);

pub const COLD_HEAT_THRESHOLD: f64 = 0.01;

pub const COLD_GC_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub const STIGMERGY_GC_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub const DISK_CONSOLIDATE_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub const DISK_CONSOLIDATE_MIN_DELTA: usize = 10_000;

pub const USER_SOURCE: &str = "user";
pub const AGENT_SOURCE: &str = "agent";
// "agent" as a source is canonically AGENT_SOURCE — do NOT add a second const
// with the same value (it silently collides in the source map).
