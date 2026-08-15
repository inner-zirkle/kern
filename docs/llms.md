# kern

> Self-learning memory substrate for AI agents. One daemon per working directory owns a knowledge graph: callers write durable facts, the graph structures and compacts itself, recall is a sub-ms graph walk with no LLM call. Local-first, no cloud. Rust.

## Core model

- **Write paths (2, caller-driven, never automatic):** MCP `ingest`, or drop a transcript into `.kern/intake/` — daemon distills it into typed claims via local LLM. LLM outage queues intake, never loses it.
- **Graph, not vector bag.** Entities = typed thoughts (Fact/Claim/Document/Question/Conclusion) with Beta-distribution confidence, access heat, content + structure vectors, bi-temporal validity window. Reason edges = typed justified links (the *why*), not similarity scores. IDs = content hashes: identical text is the same node everywhere; federation merge is set union.
- **Kern tree + gravitons.** Seed named focus attractors (name + text + mass) once; ingest routes claims to the nearest graviton; unmatched → `generic`; dense clusters get promoted and LLM-named in the background.
- **Nothing deleted.** Contradicted/updated claims are superseded. Query `as_of` past instants; `include_history` walks the chain. Update-vs-contradiction classification runs in the background tick, off the read path.
- **LLM-free recall.** Pipeline: HNSW dense + BM25 seeds → RRF → PageRank → reason-edge expansion (*why*-chains) → confidence/heat/recency/graviton boosts → filter → MMR → scored passages + chains. Caller synthesizes. Hot results < k → cold-tier backfill, flagged `cold:true`.
- **Self-compaction.** 60s tick: heat decay, clustering, LLM naming, edge enrichment, per-kern GNN structure embeddings, stigmergy GC. Cold stale non-durable thoughts spill to cold tier (capped 50k rows, FIFO) before dropping. Active Facts/Documents immune.
- **Learns from use.** Delivered results deposit heat, re-rank future recall. `degrade` down-weights bad retrieval paths.
- **Fail open.** Intake and recall no-op on any error; session always proceeds. Degradation counted in `health`: task panics/failures, cold evictions, embed-model mismatch.

## Running it

- Install: `curl -fsSL https://raw.githubusercontent.com/inner-zirkle/kern/main/scripts/install.sh | sh` · Windows `irm .../install.ps1 | iex` · or `cargo install --path .`
- Models (Ollama default; any OpenAI-compatible endpoint): `ollama pull qwen3-embedding:0.6b` + `ollama pull granite4:3b`. No answer model — recall returns passages, agent synthesizes.
- Opt in: `mkdir .kern` in project root. One daemon + graph per working directory; binary re-pins to nearest `.git`/`.kern` ancestor.
- Start: `kern --daemon`, but `kern mcp` auto-spawns one — registering MCP is usually the only step.
- State in `<project>/.kern/`: `data/data.mdb` (LMDB hot graph + cold tier), `intake/`, `kern.toml`, `data/logs/`.
- Config: `.kern/kern.toml` or `~/.config/kern/kern.toml`. Absent = defaults. Invalid = exit 78, key on stderr.
- Presets: `relaxed` (default: 0.98 dedup, 30d heat half-life), `medium`, `tight`.
- Health: MCP `health` / `kern health` — counts + degradation signals.

## Surfaces

- **MCP** (stdio + HTTP/SSE), 16 tools: `query`, `events`, `ingest`, `link`, `forget`, `forget_by_source`, `degrade`, `move`, `promote`, `health`, `graviton`, `claim_kind`, `pulse`, `gc`, `intake_drain`, `setup`. `setup` returns wiring instructions; kern never writes host config.
- **CLI** `kern <subcommand>`: reads on-disk graph directly, can race a live daemon. MCP for live state; stop daemon before `kern reembed` / `kern compact`.
- **Local RPC** socket per project, no auth (local trust).

## Federation — `building`, off by default

Opt-in LAN gossip, coordinator-free, CRDT merge over content-addressed IDs. **Unauthenticated + unencrypted** — trusted LAN only. Remote entities tagged UNTRUSTED at recall.

## Decisions

- **Stigmergy over gardening** — graph compacts itself from use signals; no manual pruning.
- **PageRank for authority** — reason-graph centrality ranks well-connected claims, unsupervised.
- **Bayesian confidence** — Beta distribution updated by support/contradiction, not a static score.
- **Edit convergence** — supersede chains + LWW/CRDT merge, no locks.
- **CRDTs over consensus** — content-hash IDs make merge conflict-free; no coordinator.
- **Knowledge, not gradients** — learns by writing structured claims, not fine-tuning weights.
- **DiskANN spill** — oversized kerns swap resident HNSW for disk-resident ANN.

## Honest limits

- No retrieval-quality claims — no LLM-free quality metric exists yet. Latency claims only.
- Reason edges are created and walkable but don't change ranking yet (tracked, xfail-tested).
- Cold-tier eviction past 50k cap is permanent (counted in `health`).
- No `kern status` — check socket or process list.
