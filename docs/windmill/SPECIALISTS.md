# Specialists

Learned expertise, written down. Each is bound by `ORACLE.md` whether run as a
subagent, persona, or read as a brief. Subagents work in-tree on disjoint file
sets; parallelize only what does not overlap.

## landscape

- **Scope:** `ROADMAP.md` — "How we supersede Zep / Mem0 / Letta / Qdrant",
  `VISION.md`.
- **Knows:** the surveyed competitor set (Zep/Graphiti, Mem0, Letta, Cognee;
  YourMemory on the decay+LoCoMo axis; mnemo, AgentDB/ruvector,
  mcp-memory-service on the Rust/embedded/MCP axis; federation has papers but
  no shipped rival), which axes kern leads on feature-wise, and that no
  quality ranking is claimable at all — the e2e scorer catches regressions and
  certifies nothing, so the claim standard is unchanged by its arrival.
- **Delegate when:** positioning, "how do we compare", or any claim that
  references another memory system — re-survey before trusting the doc,
  the field moves monthly.

## retrieval

- **Scope:** `src/retrieval/`, `src/gnn/`, `src/quant.rs`.
- **Knows:** the hybrid pipeline (HNSW/DiskANN + BM25 + GNN-blended seeds,
  edge expansion, RRF + PageRank fusion, MMR diversify — LLM-free end to end
  since 2026-07-21), int8 quantization recall parity, filtered ANN on
  `is_active`.
- **Delegate when:** recall quality, ranking, ANN structure, or any change
  that could move recall@k or query latency.

## store

- **Scope:** `src/store.rs`, persistence, cold tier, `src/crdt.rs`.
- **Knows:** the single LMDB env (heed) per data dir, single-writer +
  guarded-flush protocol, `zstd(bincode)` values, the single-version law
  (exactly one decodable format, `FORMAT_VERSION`; any persisted-schema change bumps
  it and old stores are rejected, never appended-for or migrated — guard schema
  touches with a round-trip test), content-hash ids as the merge foundation.
- **Delegate when:** persistence, schema, durability (snapshots/WAL), or
  anything holding a write guard.

## lifecycle

- **Scope:** `src/tick/`.
- **Knows:** heat deposit/pulse, half-life lazy decay, stigmergy GC and its
  Fact immunity, cold-tier spill, clustering into child kerns, graviton
  auto-promotion, the bi-temporal supersede classification that runs off the
  recall path, and that GNN training is the one thing here that is *not* a queue
  task — `src/tick/trainer.rs` owns a dedicated thread, coalesces a second
  request for a kern already waiting, catches per job, and counts what it
  refuses past `TRAIN_QUEUE_CAP` as `gnn_train_refused`.
- **Delegate when:** decay/eviction tuning, tick cadence, or anything that
  decides what the graph forgets.

## ingest

- **Scope:** `src/ingest/`, `src/llm.rs`, `src/watcher/`.
- **Knows:** the intake and its outage-safe queueing, the one-pass
  distillation into typed claims, claim kinds and gravitons, Ollama endpoints
  (reason/embed split — reason is write-path only), `num_ctx` caps,
  embed warm-keeping.
- **Delegate when:** distillation quality, claim typing, LLM latency, or
  model/endpoint wiring.

## federation

- **Scope:** `src/gossip/`, `src/trnsprt/`, `src/gossip/types.rs`, `src/crdt.rs`
  (merge semantics), `docs/site/content/docs/concepts/security.mdx`.
- **Knows:** LAN gossip heartbeats, content-addressed CRDT entity-body merge,
  multicast discovery and `network_id` pairing, which message kinds have
  receivers but no senders, and that the transport is unauthenticated and
  unencrypted today.
- **Delegate when:** anything crossing a machine boundary.

## surface

- **Scope:** `src/mcp/`, `src/rpc/`, `src/commands/`.
- **Knows:** the one-dispatch-core law (every surface goes through
  `mcp::Server::call_tool`, never a second copy), the fifteen MCP tools,
  `KernRpc` over this repo's own `service!` macro (there is no tarpc), the
  advisory writer lock, the `kern.sock` gate (one token frame verified before
  any dispatch; the client *and* the bind's `AddrInUse` arm both check the
  endpoint's owner and the serving uid, so a squatted name is refused rather
  than stood down for), and which CLI commands still touch disk behind a
  live daemon. *Corrected 2026-07-21 — this said `forget` and `degrade` were the
  only exceptions, one commit after that stopped being true; then it said five,
  one commit after the graviton and claim-kind writes started routing too; then
  it said nine, one commit after `promote` started routing too.* Ten
  commands route (`src/commands/route.rs`): `forget`, `degrade`, `promote`,
  `intake drain`, `graviton add`/`remove` and `claim-kind add`/`rm` write,
  `get` and `query` read, all ten over `call_tool` with the local pass as the
  `NoDaemon` fallback. `search` and `list` stay local **by decision** — they are
  the store-inspection commands. `ingest` and `link` still open the store
  directly and are the open half of `ROADMAP.md` item 9, blocked not on item
  24's socket auth (landed 2026-07-22) but on its unproven `principal`.
- **Delegate when:** tool schemas or CLI subcommands.
