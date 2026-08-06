# Features

A full technical scrape of everything that actually exists in the kern source
today. Organized by subsystem. For each: **what** it does, **how** it works,
**where** it lives in the code, and **gaps** (known limitations / improvement
opportunities). Version: `1.3.0`. LoC ~63.1k (raw `wc -l`, tracked + new federation files) across 180 `.rs` files.

State legend: `active` (runs today), `building` (wired but partial/unverified),
`off` (present but disabled by default).

---

## 0. Architecture at a glance

```
session delta (.txt) ──► intake ──► distill (LLM) ──► typed claims
                                                               │
                            kern tree (content-hash ids) ◄─────┘ accept()
                                   │            │
                              reason edges    access heat
                                   │            │
   ┌───────── MCP (stdio+SSE) ────┼─── RPC (typed local socket) ───┐
   │            ▲                  │                 ▲              │
   │        query pipeline ◄───────┴──────────►  recall            │
   │  (HNSW+BM25 seed → expand → RRF+PageRank → MMR → passages)    │
   │                                                                │
   │   tick queue ──► cluster / name / enrich / gc / gnn / persist  │
   │                                                                │
   └── gossip ◄── CRDT entity-body + delta merge (LAN, opt-in) ─────┘
```

One daemon per working directory (gated on `.kern/`). Everything below is the
single process that owns that directory's graph.

---

## 1. Graph data model — `active`

**What.** Two node kinds: *thoughts* (`Entity`, typed) and *justified edges*
(`Reason`). Ids are content hashes — identical content is the same node
everywhere, which is what makes conflict-free cross-node merge work.

**How.**

- `Entity` (`src/base_types.rs:277`) — typed (`Fact`/`Claim`/`Document`/
  `Question`/`Conclusion`, `src/base_types.rs:19`), weighted by
  confidence (a beta distribution stored as `conf_alpha`/`conf_beta`, read via
  the `conf_mean`/`conf_variance` methods, updated via
  `observe_support`/`observe_contradict`)
  - access `heat`. Carries a bi-temporal window (`valid_from`/`valid_to`,
  `created_at`), `status` (`Active`/`Superseded`), `superseded_by`, `statements`
  (OR-Set of text lines), two vectors (`vector` content, `gnn_vector` structure),
  and provenance (`Source` with `system`/`object_id`/`section`/`title`/`author`/
  `url`). `kind`/`source` parsed off the source string. There is **no per-row
  ACL and no user identity**: kern is a single-trust-domain store, the process
  boundary (socket `0600` + `mcp-token`) is the whole access model, and any
  multi-caller scoping is the embedding host's job (decision 2026-07-22,
  `CHANGELOG.md`).
- `Reason` (`src/base_types.rs:443`) — an edge `from`→`to` with a `kind`
  (`Similarity`/`Provenance`/`Question`/`Spawn`/`Supersedes`/`Ratification`/
  `Rephrase`, `src/base_types.rs:90-99`), its own vector (mean of endpoints), a
  `traversal_count` GCounter (`src/base_types.rs:455`), and a CRDT `score`.
  `is_enriched`/`is_remote` flags. There is no `Contradiction` edge kind —
  `Related` is a `ContradictionClass` verdict, not an edge, and a deferred
  contradiction candidate is carried by a `Rephrase` edge.
- `Kern` (`src/base_types.rs:486`) — a container node in the kern tree:
  `entities` + `reasons` maps, `children` ids, a `graviton_vec`/`graviton_text` + `mass` (default 1.0),
  radii (`inner_radius`/`outer_radius`) for acceptance gating, and an
  `access_count`. Root, named children, and unnamed (spill) children are all
  `Kern`s distinguished by `is_unnamed`/`is_named`/`has_graviton`.
- `GraphGnn` (`src/graph.rs:64`) — the whole in-memory forest: `kerns`
  map, `root`, `entity_idx` (HNSW over content vectors), `gnn_entity_idx`
  (HNSW over GNN vectors), `entity_adjacency` (reason-edge incidence),
  source routing, a Lamport clock (a plain `AtomicU64` field driven by
  `bump_lamport`/`observe_lamport`, `src/graph.rs:467`/`:474` — there is no
  `Lamport` type), a `mutation_epoch`, pending CRDT deltas, the bound embedding
  model name (`set_embed_model`/`embed_model`, `src/graph.rs:204`), and an
  optional bound `Store` (LMDB) for hot/cold tiers + disk fallback.

**Where.** `src/base_types.rs` (880 LoC), `src/graph.rs` (1325 LoC),
`src/reason.rs` (edge add/remove/move), `src/search.rs` (graph-wide
entity/reason lookup + unlocked vector search).

**Gaps.** `Entity` is a large flat struct (~30 fields); a trait-object or
sharded layout could cut serialization cost. `Kern` carries no per-kern
statistics (mean heat, fill ratio) that clustering could reuse cheaply.

---

## 2. Acceptance & routing — `active`

**What.** Decides where a new thought lives in the tree and whether it
supersedes an existing one. The core write path every ingestion funnels through.

**How** (`src/accept.rs:26` `accept()`):

1. **Dedup** — graph-wide top-1 vector search; if `score >` the preset's dedup
   threshold (0.98 on the default `relaxed`; 0.95 medium, 0.90 tight,
   `src/config_preset.rs`; `DEDUP_EF=64`), the thought is a duplicate and merges
   into the existing entity (no new node).
2. **Route** (`route_entity`, `src/accept.rs:218`) — descend from the
   target kern toward a leaf:
   - For each loaded child, route into the one whose graviton is nearest by
     effective distance `cosine_distance / mass` (`mass` default `1.0`,
     `1e-6` epsilon floor) — heavier gravitons both attract and retain.
   - At the **root** (a pure dispatcher): a no-graviton-match falls through to a
     `generic` catch-all child (empty graviton vec, never matches on similarity) —
     the root never commits entities itself.
   - At a **named** kern with a graviton: compute `acceptance_probability`
     (`src/accept.rs:895`, softmax over cosine distance vs `inner`/`outer`
     radii); below `ACCEPT_FLOOR` (0.5) → spawn an unnamed child and descend.
   - `MAX_ACCEPT_DEPTH = 64` (`src/accept.rs:17`) bounds a runaway descent.
3. **Commit** (`commit_entity`, `src/accept.rs:279`) — stamp `root_id`,
   insert into the `entity_idx`/`gnn_entity_idx`, attach a `Similarity` reason to
   the nearest existing neighbor and a `Provenance` reason to the source doc.

**Where.** `src/accept.rs` (1452 LoC). Radii defaults in `constants.rs`
(`KERN_INNER_RADIUS=0.35`, `KERN_OUTER_RADIUS=0.75`, `src/base_constants.rs:40-41`).

**Gaps.** *Both halves of this block were wrong and are corrected 2026-07-21.*
Routing does **no** index lookup per level: `route_to_child_id`
(`src/accept.rs:882`) is a linear scan over the parent's loaded, named
children, taking `cosine_distance` against each child's stored `graviton_vec`
directly. The cost is O(depth · children), not O(depth · log n), and the "cached
per-kern centroid" the old wording wanted is what `graviton_vec` already is —
root fan-out is already O(gravitons). The remaining scaling question is the
per-parent fan-out itself, not an index.

Unnamed children are **not** unbounded on the routing path: `route_entity` goes
through `get_or_spawn_unnamed_child` (`src/accept.rs:787`), which reuses the
single holding-pen child and auto-loads an evicted one rather than respawning it
(three tests, both holding pens: `src/accept.rs:921`, `:940`, `:963`).
Growth comes only from tick clustering, which deliberately spawns one *distinct*
child per spawnable cluster (`spawn_child_clusters`, `src/tick.rs:225`) —
bounded per pass by the cluster count, not by anything per parent.

---

## 3. Bi-temporal supersede & contradiction — `active`

**What.** Conflicting claims *supersede* rather than delete. The old revision
stays as history with a stamped `valid_to`; `query` can recover the past via
`as_of` or walk the supersede chain via `include_history`.

**How.**

- `supersede_by_contradiction` (`src/accept.rs:562`) — inserts the new
  thought, sets the old `status=Superseded`, `superseded_by=new_id`, and
  `stamp_invalidated(now, new_valid_from)` so the window closes exactly when
  the new claim became true. Removes the old id from both vector indexes (so it
  stops seeding) but keeps it in the kern for history. Adds a `Supersedes`
  reason edge with the averaged vector.
- Classification is LLM-driven (`classify_prompt` `src/accept.rs:693` /
  `parse_contradiction` `src/accept.rs:703`) and **fails open to `Related`**
  (co-exist) — the conservative choice that never loses data. Driven from the
  tick's `do_classify_contradiction` task (`src/tick/tasks.rs:114`) so recall
  stays LLM-free at query time.
- `is_valid_at(instant)` / `valid_from_or_created()` on `Entity` answer
  point-in-time membership; the query layer's `include_history` walks the
  `superseded_by` chain.
- The three stamps survive the **cold tier**. A spilled row is a `ColdRow`
  (`src/base_store.rs`) = `Entity` ++ `StoredTemporal`, written under
  `FORMAT_VERSION` and decoded strictly, never by parse-sniffing (`decode_cold`) — a
  truncated value errors instead of silently degrading to a stampless `Entity`. So a
  cold-recovered revision keeps `valid_from`/`valid_to`/`invalidated_at` and
  `is_valid_at` answers over the cold tail exactly as it does over the hot graph.

**Where.** `src/accept.rs`, `src/base_types.rs` (temporal helpers),
`src/base_store.rs` (cold-tier round-trip), `src/tick/tasks.rs` (background
classification).

**Gaps.** Classification runs once per near-duplicate pair on the tick; a
re-classify when either side changes isn't triggered. No UI/MCP tool exposes
the history chain directly beyond `include_history`.

---

## 4. Retrieval pipeline — `active`

**What.** The hybrid query engine. Hand-rolled end to end (no external ANN or
rerank lib). This is the product's core IP.

**Stages** (`retrieve_profiled`, `src/retrieval/query.rs`, each checkpoint
profiled via `src/profile.rs`):

| # | Stage | File | What happens |
| --- | ------- | ------ | -------------- |
| 1 | **Seed dense** | `retrieval/seed.rs` | HNSW top-`k` over a 0.4/0.6 blend of content + GNN vectors (`Weights::for_mode` per `Mode` Hybrid/Vector/Lexical/Reason). Plus `seed_important` — an O(N) scan feeding access/recency (`IMPORTANT_ACCESS_THRESHOLD=3`, `IMPORTANT_MIN_COSINE=0.20`) into both the dense merge and RRF, run once. |
| 2 | **Seed lexical** | `retrieval/seed.rs:86` | BM25 (`LexicalIndex`) candidate list, fused via RRF when `mode==Hybrid`. |
| 3 | **Fuse (RRF)** | `retrieval/fuse.rs` | Reciprocal-rank fusion of dense + lexical + important lists with mode weights. |
| 4 | **PageRank** | `retrieval/pagerank.rs` | Centrality weighting of the fused seeds over the reason graph. |
| 5 | **Expand** | `retrieval/expand.rs:178` | Walk reason edges out from seeds (`PathChain` recording the *why*), scoring neighbors (`score_neighbor`) — plus bounded traversal credit: each examined edge pays its far endpoint `source_score × edge_evidence` (×`traversal_credit_weight`, capped at `traversal_credit_cap`, clamped below the strongest voucher's walk score), which is how a linked neighbour sharing no words with the query reaches the top ranks without ever outranking a direct match. |
| 6 | **Merge** | `retrieval/merge.rs` | Combine seeds + expanded neighbors into `ScoredEntity` list. |
| 7 | **Boosts** | `retrieval/score.rs` | `apply_boosts`: (confidence × score + **QBST** access/recency boost (`qbst`, capped at 0.1, 24h half-life) + `fact_score_boost` (0.3) for Facts) × `source_trust`. `source_trust` is a `RetrievalConfig` map keyed on `Source::scheme()` — `file`, `ticket`, `session`, `agent`, `inline` — empty by default, absent key exactly `1.0`, so an unconfigured kern scores bit-identically. It weights the CHANNEL, never the author: `kern ingest` and an MCP agent's default ingest both write `inline` (`ROADMAP.md` item 20). An unknown key is a `validate` error, not a silent no-op. |
| 7b | **Gravity** | `retrieval/gravity.rs` | Query-time graviton pull: `score += gravity_weight (0.15) * max_over_gravitons(mass * max(0, cos(entity, graviton_vec)))`. Max, not sum — overlapping gravitons never double-count. `gravity_weight=0` disables (early return, zero cost); no gravitons → no-op. Latency only, from the bench deleted in `8d8b19e` and not reproducible: ~+7% p50 with 5 gravitons. No quality claim accompanies it — the retrieval-quality half of that bench is withdrawn under the claim standard (`ROADMAP.md` — "no quality claim of any kind"). |
| 8 | **Filter** | `retrieval/score.rs` | `filter_delivery`: drop superseded; floor at `retrieval.min_deliver_score` (default `0.0` — off); cap at `delivery_cap` = `retrieval.max_deliver_results` (default `25`), or `mmr_pool_size=50` when MMR is on. Both are config fields (`src/config_retrieval.rs:56-57`), not constants. `delivery_cap` is a named function because the CLI reads it too — `cmd_query` sends it as `k` when it routes to a daemon, so the routed and local reads deliver the same number of hits. Query options (source/kind/scheme/time/min_conf) go through `matches_filter` (`retrieval/score.rs`), the single predicate shared with pre-filtered ANN search. |
| 9 | **Dedup by section** | `retrieval/diversify.rs:6` | Collapse near-duplicate sections. |
| 10 | **MMR** | `retrieval/diversify.rs:46` | Maximal-marginal-relevance diversification so the `k` results actually differ. |
| 11 | **Deliver** | `retrieval/query.rs` | Passages + enriched edges + `format_chains` chain text (`QUERY_MAX_CHAINS=5`), remote entities tagged UNTRUSTED for the synthesizing caller. Chains answer an active filter too (`retrieve`, same file): a chain renders the TEXT of every entity on it, so filtering only the results left it as a second delivery channel — one touching a withheld entity is dropped whole, since a chain with a hole still says the withheld thought exists and what it connects. The whole read path is LLM-free by design (2026-07-21): the calling agent synthesizes; an in-kern small-model answerer set the quality ceiling and made retrieval untunable. |
| 13 | **Cold backfill** | `src/mcp/tools_query.rs:212` | If hot returns `< k`, cold-tier hits (brute-force `Store::cold_search`, `src/base_store.rs:629`) fill remaining slots, flagged `cold:true` — each first put through `matches_filter`, because `cold_search` is a raw cosine scan that answers no predicate of its own and an unfiltered fill made spilling an entity the way around every filter the hot path enforces. Skipped on the exact-text fast path, which never embedded a query vector. <!-- docs-check: anchor-ok --> |
| 14 | **Access stamping** | `retrieval/score.rs` | Heat deposits off the hot path: `score::commit_access` stamps delivered hits; the tick's `CommitAccess` task calls `score::commit_access_ids`. |

**Where.** `src/retrieval/*` (5182 LoC, 9 files). Entry: `retrieval::query`
(one-shot CLI) and `retrieval::query_locked` (daemon, holds read lock only for
the graph phase; every LLM call runs unlocked).

**Gaps.**

- The O(N) importance scan runs every retrieve; at scale it should be indexed.
- RRF weights and mode blends are config but not auto-tuned.

---

## 5. Indexes — `active`

**What.** Three hand-built approximate/brute indexes backing seed + dedup +
cold backfill.

**How.**

- **HNSW** (`src/hnsw.rs`, 1042 LoC) — id-stable, deterministic-build
  graph ANN. `insert` (`:166`) / `delete` (`:136`) / `search` (`:248`) /
  `search_filtered` (`:273`, pre-filtered ANN that shares one filter predicate
  with post-filtering). Quantization-aware: stores `QuantizedVec` (int8) when
  configured. `structure_digest` for parity checks.
- **DiskANN** (`src/diskann.rs`, 665 LoC) — disk-resident graph index.
  `build_and_save` (Params `r=32, build_l=64, alpha=1.2`) writes
  `meta.bin`/`vectors.bin`/`graph.bin`; `DiskIndex::open`/`search` (`:385`) /
  `search_hits_filtered` (`:400`). Selected when a kern exceeds `disk_threshold`.
- **BM25 LexicalIndex** (`src/lexical.rs:62`) — in-RAM inverted index,
  `k1`/`b` tunable (`set_bm25_params`), `rebuild_from_graph` (`:155`),
  `search`/`search_filtered` (`:100`/`:105`). One document per entity id, built
  by `entity_document` (`:15`) from the entity's statements plus every alternate
  wording a dedup merged onto it, so either wording matches and the entity still
  returns once.
- **VectorBackend** (`src/vector_backend.rs`) — enum switch
  (`Resident(HnswIndex)` | `Disk(DiskIndex)`) unifying the search API so the
  retrieval layer is backend-agnostic.

**Where.** `src/{hnsw,diskann,lexical,vector_backend,search}.rs`.

**Gaps.** HNSW delete is not a tombstone — it scrubs inbound edges, nulls the
node and queues the slot; one `scrub_pending` pass per sweep recycles every slot
deleted since the last one, so the cost is the scan, not accumulation.
DiskANN is build-once; incremental updates funnel through
`consolidate_disk_index` on the tick. Lexical index is RAM-only.

---

## 6. Quantization — `active`

**What.** int8 (and float fallback) vector storage + distance, cutting vector
memory ~4×.

**How.** `QuantizationMode` (`None`/`Int8`/`Binary`, `src/quant.rs:7`; `Binary`
is implemented and tested but deliberately excluded from `parse` (`src/quant.rs:16`),
so it is not user-selectable — recall floor is too low without rescore),
`QuantizedVec::encode`/`decode`, `quantized_cosine_distance` (`src/quant.rs:159`)
falling back to a private `float_cosine_distance` (`:171`) across mismatched
modes. `INT8_MAX_ABS=127`. The HNSW index picks the mode at build; both resident
and disk backends honor it.

**Where.** `src/quant.rs` (476 LoC).

**Gaps.** No int4 / product-quantization path. Scale is fixed at encode time.

---

## 7. Persistence (LMDB) — `active`

**What.** One ACID LMDB env per data dir (`data.mdb` + `lock.mdb`); hot graph
and cold tier live together. Readers never block, writers serialize.

**How.**

- `Store::open` (`src/base_store.rs:314`) opens the env (`heed` 0.20);
  `StoredKern`/`StoredVec`/`StoredTemporal`/`ColdRow` are the on-disk bincode
  shapes, each value a version byte followed by a `zstd` frame
  (`encode_at`/`strip_version`, `src/base_store.rs`), vectors int8. Exactly one
  live format, `FORMAT_VERSION`; any other version byte is rejected, never
  mis-decoded and never migrated.
- **Guarded flush** (`Store::flush_guarded` `src/base_store.rs:571`,
  `persist::flush_guarded` `src/persist.rs:129`) — a snapshot carries an
  expected `mutation_epoch`; if disk advanced under us (another writer /
  external edit), the flush is *refused*, the disk rows are *absorbed* back
  (`merge::absorb_graph`), and the flush retries. Prevents a stale in-memory
  snapshot from clobbering newer on-disk state.
- **Embedding stamp.** The store records the model and vector dimension it was
  built with (`EmbedStamp`, its own meta key so an unstamped store reads as
  *unknown*, never as a mismatch). `check_embed_stamp` (`src/base_store.rs:417`)
  runs at open via `persist::check_graph_stamp` (`src/persist.rs:93`),
  wired from `commands::bind_embed_model`: an **unstamped** store adopts the
  configured model and says so once; a **differing** model or dimension sets a
  durable `embed_mismatch` flag, logs through a `LogThrottle`, and leaves the
  stored stamp intact because it still describes what is on disk. An unreadable
  stamp is treated as unknown, not as unstamped — adopting over it would erase
  the identity of the stored vectors. `kern reembed` stamps the model it
  *actually embedded with*, not the configured one
  (`src/commands_reembed.rs:66-80`), so `health` can never report a false identity.
- **Query dimension guard** (`src/search.rs:23` `dim_guard`) — `cosine`
  truncates to the shorter side, so an off-model query vector would score noise
  and rank it as recall. Every graph vector search checks the query dimension
  against the indexed one first. Fail-open by design: a rejected query returns
  no hits rather than panicking, but it is *counted*
  (`search::query_dim_rejected`, `src/search.rs:15`) and logged throttled,
  because the silent no-op is what let the mismatch hide.
- **Cold tier** — `cold_spill` (`src/base_store.rs:624`) / `cold_get` (`:636`) /
  `cold_all` (`:649`) / `cold_put_all` (`:666`) / `cold_search` (`:684`). Rows are
  stored without their vector; the vector lives alone in `COLD_VEC_DB` (`:26`), so
  the full-tier scan scores off raw floats and decodes only the k winners, and
  `cold_get`/`cold_all` rejoin the halves. Bounded by `COLD_MAX_ENTRIES = 50_000`
  — *softly*: both write paths (`:632`, `:676`) call `cold_cap_amortized` (`:728`),
  which skips the scan until the tier passes `max + COLD_CAP_SLACK` (1024, `:20`);
  only then does `cold_cap` (`:739`) sort by `created_at` and cut back to `max`. A
  drop is unrecoverable, so `cold_evicted` (`:780`) feeding `health` is its trace.
- **Compaction** (`compact_dir`, `src/base_store.rs:818`) — the only way to
  shrink LMDB's high-water mark; writes a fresh env to a tmp file then
  `swap_compacted` renames with retry. Requires exclusive access (run offline).
- **Snapshots** — `snapshot_for_flush` (`src/persist.rs:154`) /
  `FlushSnapshot` capture a consistent point-in-time; the maintenance tick runs
  a mutation-epoch-gated snapshot so crash loss is bounded to one tick interval.

**Where.** `src/base_store.rs` (1611 LoC), `src/persist.rs` (565 LoC),
`src/search.rs` (dimension guard), `src/store.rs`
(per-cwd `Registry` of open stores).

**Gaps.** Single-writer is enforced, not assumed — `src/lock.rs` is an advisory
lock `reembed`, `gc` and `compact` claim or refuse — but `cmd_hub_merge`
(`src/commands_admin.rs:1002`) and `maybe_self_heal_store` (`src/commands.rs:444`)
still `save_graph_unguarded` holding none. No WAL but LMDB's; compaction is offline.

---

## 8. Intake & distillation (self-learning) — `active`

**What.** A conversation delta (`.txt`) dropped in `.kern/intake/` is drained,
run through one LLM pass, and turned into typed claims ingested into the graph.
Nothing is lost on an LLM outage — the delta stays queued until it succeeds.

**How.**

- **Intake** (`src/ingest/intake.rs`) — `run()` (`:315`) polls `.kern/intake/`,
  `extract_claims` (`:13`) distills, `archive`/`finalize` (`:55`/`:90`) move
  processed deltas to a `done/` dir, `prune_done` (`:99`) ages them out.
- **Distill** (`src/ingest/distill.rs`) — a structured prompt asks the LLM for
  a JSON array of `{text, kind, valid_from?}` where `kind` is one of the 7
  built-in claim kinds (`DEFAULT_KINDS`, `src/ingest/distill.rs:9`) or a
  registered one (`root.claim_kinds`, offered to the LLM by `spawn_intake`'s
  kinds closure). The prompt names today's date (UTC `YYYY-MM-DD` via
  `date_string` in `src/time.rs`, from a `now: SystemTime` param callers
  pass as `SystemTime::now()`), so a relative-date phrase ("last Tuesday") in
  the delta resolves to an absolute ISO8601 `valid_from` rather than storing
  unresolved (item 50, 2026-07-22).
  `Some([])` = nothing worth keeping (archive); `None` = no LLM
  output (transient outage, retry). `parse_claims` is lenient (finds the JSON
  array anywhere in the output).
- **Worker** (`src/ingest/worker.rs`) — async job queue bounded at
  `QUEUE_CAP` = 64 with no detached send behind it. Three offers: `enqueue`
  refuses when full (`None`, counted as `ingest_queue_refused`), `submit` awaits
  capacity for a producer that can be slowed instead (the file watcher), `run`
  awaits the outcome. The fill is a gauge, not a counter: `queue_depth`
  (`src/ingest/worker.rs:148`) reads the channel's own occupancy and surfaces as
  `ingest_queue_depth` on every health surface (ROADMAP item 30). Owns the embed + accept path. Defers question/contradiction follow-ups to
  the tick via callback closures (`DeferQuestionsFn`/`DeferContradictionFn`).
- **Embed** (`src/ingest/embed.rs`) — batches texts to the embedding endpoint.
- **Dedup** (`src/ingest/dedup.rs`) — `find_duplicate` at the preset's dedup
  threshold (0.98 on the default `relaxed`), `update_existing_entity`.
- **Place / split / direct** — `place.rs` builds chunk `Entity`s
  (`build_chunk_entity`, `chunk_source_id`), `split.rs` chunks by free-text hint
  (LLM-assisted when given), `direct.rs` handles `.kern/intake/direct/` synchronous
  ingest (`drain_direct_once`).
- **Per-source TTL** (`src/ingest/config.rs`) — `ingest::Config` carries a
  `valid_until`; `valid_until_from_retention(secs)` is the one conversion from
  the caller's duration to that absolute instant, so the four entrances cannot
  drift. `0`/absent = no TTL; an overflowing duration errors, never a silent
  no-TTL. `new_statement_entity` stamps it on both the document and the chunk
  path (`place.rs:106`, `:242`), where the existing LWW lamport/producer
  stamping and pending delta finally have a writer; the reader half is
  `score::drop_expired`. `DirectJob` carries the resolved instant, which
  `drain_direct_once` overlays per job. The two entrances with no caller to pass
  a flag take a standing policy instead: `[intake]` / `[watcher] retention_secs`
  via `Config::with_retention`, per drain pass and per record so no deadline
  dates to daemon boot, validated at load, never the preset-owned `[ingest]`.
- **File watcher sink** (`src/ingest/file_watcher.rs`) — `KernFileWatcherSink`
  adapts the watcher into ingest jobs, stamping `[watcher] retention_secs`. Since item 30 it parks each record as a durable `DirectJob` first (`:104`, gated on `intake.enabled` — the same flag that spawns the drain) and falls through to `Worker::submit` (`:128`) only when that write fails, so a record still in flight when the daemon dies is re-offered by the next drain instead of lost.
- **Outcome** (`src/ingest/outcome.rs`) — `OutcomeStatus` (`Committed`/`Partial`/`Deduped`/`Failed`, `src/ingest/outcome.rs:2`),
  `FailureReport::document_permanent` for non-retryable errors.
- **Status & sidecars** (`src/ingest/intake_status.rs`) — every path that leaves
  a delta queued writes why to `<intake>/errors/<name>.txt` through
  `record_stuck`, cleared on the next success; `scan` reports pending (age +
  last error), quarantined and done. Without this a delta retried forever is
  indistinguishable from one not yet picked up.
- **CLI** (`src/commands_intake_cmd.rs`) — `kern intake` (alias `intake
  status`) prints that report; `kern intake drain` forces one pass. It routes to
  the daemon's `intake_drain` tool when one is serving — one drainer, never two
  distilling the same file — and falls back to `drain_locally`, an in-process
  `intake::drain_now` flushed through the same guarded retry as `cmd_ingest`.
  Both share `drain_once` with the daemon loop, and both print the same tail.

**Where.** `src/ingest/*` (3583 LoC, 13 files). Spawned by `spawn_intake`
(`src/commands.rs`); driven manually by `src/commands_intake_cmd.rs`.

A **deduped** ingest carries its retention too. `accept::merge_valid_until` is
the one place a `valid_until` decision is written, and all three placement
outcomes reach it: the `find_duplicate` gate in `place.rs` and `commit_entity`'s
`dup` branch in `accept.rs` both funnel through `merge_duplicate`, and a fresh
placement calls it directly *after* accept, on the id that actually entered the
graph. The rule is `min` with `None` as +∞ (`accept::resolve_valid_until`): a
TTL bounds a lifetime, so merging two bounds keeps the **lower** one, which is
commutative and idempotent and therefore converges under any replay order. A
fresh lamport/producer is stamped and a `ValidUntil` delta queued only when the
stored deadline actually moves or was never stamped, and always against the
**survivor's** id — the discarded incoming entity never gossips one, is never
acked back to the caller, and never enters the lexical index. **Known cost:**
ingest can only ever *shorten* a deadline. There is no way to lengthen one
through ingest; that needs an explicit update path, or `forget` + re-ingest.

**Gaps.** Distill prompt is one-shot; long deltas may truncate. No per-kind
prompt tuning. Dedup threshold is global, not per-kind. Retention now reaches
all four entrances, but the file-watcher one is unit-covered only — nothing in
`tests/e2e/` starts a watcher, since it is off by default — and `DirectJob` carries
`valid_until` but drops `valid_from` (item 90).
Separately, a near-duplicate's alternate wording survives only on a `Rephrase`
reason and is indexed neither lexically nor densely (item 94).

---

## 9. Self-compaction (tick) — `active`

**What.** A background task queue drives heat decay, clustering, naming,
enrichment, GC, GNN propagation, and persistence. An idle daemon still
maintains itself.

**How.**

- **Queue** (`src/tick/queue.rs`) — bounded (`TICK_QUEUE_CAPACITY=512`) mpsc
  with backpressure, `TaskKind` enum (`src/tick/queue.rs:8`: Cluster/Name/
  Enrich/ResolveQuestion/SeedQuestions/ClassifyContradiction/Persist/
  GnnPropagate/StigmergyGc/Reembed/DiskConsolidate/IdleSweep/CommitAccess).
  Records per-task latency, pending/done metrics, and two separate degradation
  counters: `panics` (a task that died) and `failures` (a task that ended early
  and re-enqueues forever), each keeping the most recent `TaskFault`
  (`src/tick/queue.rs:38` — kind, kern, message).
- **Driver** (`tick::start`, `src/tick.rs:38`) — one async task drains the
  queue and dispatches via `process_task`. Every task runs inside `run_guarded`
  (`src/tick.rs:65`), which wraps `process_task` in
  `catch_unwind(AssertUnwindSafe(…))`: a panicking maintenance task now costs
  one task, not decay/GC/persist/clustering/idle-sweep for the rest of the
  process's life. The panic is logged with its kind and kern and recorded via
  `Queue::record_task_panic`; the loop resumes over state the dead task may have
  half-written, which is exactly what the error line says (the graph lock does
  not poison, so `AssertUnwindSafe` is deliberate). A panicking task's duration
  is *not* fed to `task_avg_ms` — averaging work that never finished would make
  the metric lie as failures climb. `tick_sync` (`src/tick.rs:332`) is the
  synchronous one-shot variant; `enqueue_all` (`:323`) fans a Cluster task out
  to every non-empty kern.
- **Maintenance tick** (`spawn_maintenance_tick`, `src/commands.rs`) — periodic
  driver at `TICK_INTERVAL_SECS=60` (0 = event-driven only): pulses the root,
  gates GC and disk consolidation on clock validity + elapsed interval
  (`pulse::should_run_gc`, `src/tick/pulse.rs:52`), enqueues persist.
- **Pulse** (`src/tick/pulse.rs`) — `pulse` (`src/tick/pulse.rs:15`) fans Cluster tasks out from the root,
  decaying strength by `PULSE_DECAY=0.5` per level; below `PULSE_THRESHOLD=0.05` it
  stops, covering 5 levels. Deposits **no** heat, takes the graph by shared reference.
  Heat decays lazily by age (`heat::decayed`, half-life based), *not* per tick.
- **Cluster** (`src/tick/cluster.rs` + `tick::do_cluster`) — `vector_cluster`
  (`src/tick/cluster.rs:13`) samples up to `TICK_MAX_CLUSTER_SAMPLE=200`
  entities and groups them; a cluster
  that is `≥ KERN_MIN_CLUSTER_SIZE=10` and `cohesion ≥ KERN_COHESION_THRESHOLD=0.60`
  and not a core cluster spawns a distinct unnamed child and migrates its
  members. Unnamed kerns never spawn (bounds descent). Empty unnamed children
  are evicted back to the parent each pass.
- **Name** (`do_name`, `src/tick/tasks.rs:236`) — LLM names an unnamed kern from
  its centroid (`cluster::graviton_prompt`) once it crosses the naming
  thresholds (`KERN_NAMING_COHESION_THRESHOLD=0.50`,
  `KERN_NAMING_MIN_CLUSTER_SIZE=5`).
- **Enrich** (`do_enrich`, `src/tick/tasks.rs:315`) — LLM writes the explanatory
  text for an un-enriched reason edge.
- **Resolve question** (`do_resolve`, `src/tick/tasks.rs:383`) — open `Question`
  edges (`to` empty) get answered by retrieval; if a hit scores above
  `QUESTION_RESOLVE_THRESHOLD=0.80` the edge is closed.
- **Seed questions** (`do_seed_questions`, `src/tick/tasks.rs:42`) — broadcasts
  open questions to peers (federation).
- **Commit access** (`do_commit_access`, `src/tick/tasks.rs:455`) — flushes
  queued access-count/heat updates.
- **Idle sweep** (`src/tick/idle.rs`) — graph-global; unloads kerns idle past
  `tick.kern_idle_timeout_secs`. Residency, not forgetting: an unloaded kern is
  persisted first and reloads on next access.
- **Persist / reembed / disk consolidate** — `do_persist`
  (`src/tick/tasks.rs:466`), `do_reembed` (`src/tick/tasks.rs:498`),
  `do_disk_consolidate` (`src/tick/tasks.rs:451`).

**Where.** `src/tick/*` (3589 LoC, 8 files) + `src/tick.rs` (1070 LoC) — remeasured 2026-07-22, the old 2912/893 had drifted ~660 and ~177 lines behind the tree. `trainer.rs` is the one that is not a queue task: GNN training runs on its own thread.

**Gaps.** `KERN_CAP_DISABLED` (`src/base_constants.rs:30`) is a **kern-eviction**
sentinel, not an entity cap. Its two readers are `max_loaded_kerns` (how many
kerns stay resident, `enforce_kern_cap`, `src/graph.rs:227`) and
`disk_threshold` (the per-kern entity count that triggers a DiskANN spill,
`src/graph.rs:296`). `max_kerns` now defaults to **128** (2026-07-22, item
83): a conservative resident bound — eviction is proven safe (`get_mut`
auto-loads; `spawn_unnamed_child_under_cap_keeps_the_child_in_parent_children`),
128 bounds the pathological case, and an explicit `usize::MAX` opts out.
`disk_threshold` still defaults to `KERN_CAP_DISABLED` until item 75 (DiskANN
crash consistency) closes. A per-kern *entity* cap does not exist for local
kerns at all; the only one in the tree is `GOSSIP_REMOTE_KERN_ENTITY_CAP` for
`remote-*`.
Clustering is vector-only; no semantic/structural features. Naming/enrich are
LLM-cold per kern. Only `GnnPropagate` reports a *contained* failure today
(`src/tick/gnn_propagate.rs:57`); every other task's early return is still
invisible except as work that did not happen.

---

## 10. Stigmergy GC — `active`

**What.** Cold, stale, non-durable thoughts evict themselves; **Facts and
Documents are immune while Active** (immunity is revoked once superseded);
evictions spill to the cold tier before dropping (spill-before-drop). Spill is
lossless out of RAM, not lossless overall — the cold tier is capped at
`COLD_MAX_ENTRIES = 50_000` and `Store::cold_cap` (`src/base_store.rs:739`)
deletes the oldest rows past it, and with no store bound `run_gc` drops the
victim outright.

**How.** `stigmergy::run_gc` (`src/tick/stigmergy.rs`) collects victims per
kern where `is_cold_victim` holds (heat below `COLD_HEAT_THRESHOLD=0.01` *and*
not accessed within `COLD_GC_AGE = 7 days` *and* not an Active `Fact`/`Document`),
spills the whole list to the cold store in ONE transaction, then `remove_entity`.
A failed batch retries per victim, so a bad row alone stays hot. Runs on the
maintenance tick gated by `STIGMERGY_GC_INTERVAL = 1 hour` and clock validity.

Past the cold cap the drop is **counted, not silent**: `cold_cap` increments
`Store::cold_evicted` (`src/base_store.rs:718`) per deleted row and warns once
per sweep, and `health` reports that total on all three surfaces (MCP JSON,
`HealthRes`, `kern health` — the daemon's, item 100). The cap stays intentional.

**Where.** `src/tick/stigmergy.rs`, `src/reason.rs` (`remove_entity`
cascade-deletes its edges), `src/base_store.rs` (cap + eviction counter).

**Gaps.** Victim selection is per-kern linear. No priority/age queue. Cold tier
is brute-force search only, and an entity dropped past the cap is gone — the
counter records that it happened, nothing recovers it.

---

## 11. GNN (learned structure re-embedding) — `active`

**What.** A from-scratch graph neural network that re-embeds each thought from
*graph structure* (not just content), so the dense seed blends content + structure.
Trained per-kern, off the tick loop on a dedicated thread (`src/tick/trainer.rs`).

**How.**

- **Graph** (`src/gnn/graph.rs`) — `add_node` (`:39`) / `add_edge` (`:51`) /
  `add_self_loops` (`:111`) / `feature_matrix` (`:83`) / the symmetric normalized
  adjacency, sparse (`:178`, what trains) and dense (`:134`, its reference).
- **Layers** — `LinearLayer` (`src/gnn/layer.rs:17`), `GCNLayer`
  (`src/gnn/gcn.rs:10`: linear + optional `LayerNorm` + `Activation`),
  `LayerNorm` (`src/gnn/norm.rs:5`). No dropout ships.
  `Activation` (`src/gnn/activation.rs:27`) is exactly two variants — `Relu` and
  `Sigmoid` — each with its derivative. Nothing else is implemented.
- **Model** (`src/gnn/model.rs:9`) — `Model::new(layers, out_layer)` over a
  `Vec<GCNLayer>` plus an optional `LinearLayer` head; `parameters(_mut)`,
  `param_grads(_mut)`, `zero_grads`. Manual autograd via `backward.rs`
  (`GraphLayer`/`BackwardGraphLayer` traits).
- **Fallible forward/backward.** `Model::forward` (`src/gnn/model.rs:19`) and
  `Model::backward` (`:30`) return `Result<_, GnnError>` and every layer call
  inside them is a `try_` variant, so a shape or missing-forward-state error
  propagates instead of silently zeroing. `GnnError::MissingForwardState`
  (`src/gnn/mod.rs`) is the specific case a backward-without-forward raises.
- **Training** (`run_learned_propagation`, `src/gnn/propagate.rs:67`) — builds
  a `GnnSnapshot` (features + positive reason edges + last weights), samples
  negative edges, trains a 2-layer GCN (`dim → (dim/2).clamp(16,256) → dim`) for
  `DEFAULT_TRAIN_EPOCHS=24` with `Adam` (`DEFAULT_TRAIN_LEARNING_RATE=0.01`) on
  the link-prediction gradient (`link_prediction_grad`, `src/gnn/loss.rs:34`;
  `link_prediction_loss` at `:13` is the scalar form). Output embeddings blended
  with input features at `DEFAULT_SELF_WEIGHT=0.6`, normalized, written back as
  `gnn_vector`. Requires `≥ DEFAULT_MIN_THOUGHTS=128` thoughts. The whole
  function returns `Result<PropagationResult, String>`: every epoch's forward and
  backward, the inference forward, and the weight marshal are `?`-propagated, so
  **a failed propagation writes nothing** — no half-trained embeddings, no
  weights that produced them.
- **Failure surfacing** (`src/tick/gnn_propagate.rs:50-57`) — on `Err` the tick
  logs `kern.gnn` with the kern id and calls `Queue::record_task_failure`, which
  `health` reports as `task_failures` / `last_task_failure`. Embeddings and
  weights are left untouched.
- **Success surfacing** (`src/tick/gnn_propagate.rs:40-45`) — on `Ok` the tick
  logs `kern.gnn` at INFO with the kern id and `nodes`, the number of embeddings
  the run produced. It is the only trace a *completed* propagation leaves outside
  the graph: `gnn_vector` is dropped on persist, so nothing on disk can say the
  GNN ever ran. `tests/e2e/test_gnn_recall.py` gates on this line.
- **Optimizers** (`src/gnn/optim.rs`) — `Adam` (`:14`) behind an `Optimizer`
  trait. No SGD ships.
- **Persist** (`src/gnn/persist.rs`) — `marshal_weights` (`:52`) /
  `unmarshal_weights` (`:69`) to and from a byte blob carried on the snapshot,
  versioned `WEIGHT_FILE_VERSION=1` with typed `PersistError` variants for
  version, parameter-count and per-parameter shape mismatch. There is no
  separate weight *file* API — the blob rides the kern.
- **Tensor** (`src/gnn/tensor.rs`) — own 2D tensor + matmul. `SparseMatrix` (`src/gnn/sparse.rs:14`) is the CSR counterpart the GCN aggregation runs on; its columns ascend inside a row, so `matmul` (`:53`) and `transpose` (`:84`) visit the same nonzeros in the same order the dense product does and the swap is bit-identical rather than merely close.

**Where.** `src/gnn/*` (2766 LoC, 14 files). Driven by
`tick::gnn_propagate::do_gnn_propagate`.

**Gaps.** Training is linear in edges since 2026-07-22 — 73.4s → 11.6s at 4096 measured back to back under load, 6.6s idle (`tests/gnn_scale.rs`); off the tick since 2026-07-21 (`src/tick/trainer.rs`). No GPU.
Weights are per-kern, not shared across the tree. Link prediction only — no node-classification objective. **A propagation is reproducible since 2026-07-22** (`ROADMAP.md` item 102): one seed derived from the sorted node ids (`gnn_seed`, `src/tick/gnn_propagate.rs:182`) drives both weight init and negative-edge sampling (`src/gnn/propagate.rs:80`), and the two `HashMap` walks that outranked it are sorted — the snapshot's node order (`src/tick/gnn_propagate.rs:71`) and the `updates` write-back that fixes HNSW insert order (`src/tick/gnn_propagate.rs:212`) — so the same corpus re-embeds identically in every process and `tests/e2e/test_gnn_recall.py` prints the same numbers on every run rather than scoring a draw.
*Corrected 2026-07-21:* a repeatedly failing
propagation does **not** re-enqueue every tick. `GnnPropagate` is enqueued only
when `do_cluster` did structural work (`if did_structural_work`, `src/tick.rs:190`),
so a quiescent kern retries nothing; the climbing `task_failures` count
(`src/tick/gnn_propagate.rs:57`) is still the only visibility when it does.

---

## 12. MCP surface — `active`

**What.** Model Context Protocol server (stdio + HTTP/SSE) exposing the graph
to external clients (Claude, Cursor, etc.). Protocol version `2024-11-05`.

**Tools** (16, in `src/mcp/tools*.rs`, dispatched in `mcp.rs` `call_tool`):

| Tool | File | Purpose |
| ------ | ------ | --------- |
| `query` | `tools_query.rs` | Hybrid search, LLM-free; the caller synthesizes. Filters: `mode`/`kind`/`source`/`scheme`/time range/`min_conf`/`valid_at`/`as_of`; `include_history` for supersede chain; `exclude_pending` drops rows a `[ingest] review_policy` is still holding (opt-in, with a CLI flag of its own, `kern query --exclude-pending` — which is what makes the review lifecycle e2e-measurable). Returns edges **and path chains**, and `id` resolves a prefix and the cold tier (`entity_detail_by_id`) — both widenings exist so a CLI `query`/`get` routed through the daemon answers with what the local path answers. An `id` read runs the **same** filters: `src/mcp/tools_query.rs:137-157` builds `QueryOptions` first and puts the resolved row through `retrieval::score::matches_filter`, so `query {id, kind: "claim"}` on a `Fact` answers `thought not found`. A bare `query {id}` filters nothing — `QueryOptions::default()` leaves `valid_at`/`as_of` unset — which is what keeps an expired row served-and-flagged (`expired`/`valid_until`, `entity_detail` `src/mcp/tools_query.rs:372`, stamped `:410-412`) rather than hidden. |
| `events` | `tools_events.rs` | Read-only change feed for pollers: `created` for each entity ingested after an opaque cursor and `superseded` for each revision invalidated after it, ordered ascending — derived from the bitemporal stamps the graph already keeps, mutating nothing. `since` = the `cursor` a prior call returned (0/absent = from the beginning) resumes without gap or overlap; `limit` bounds the batch. The Event source a ctrl Watcher polls. `degraded`/`forgotten` are declared on the wire contract but never emitted — a forgotten entity leaves no resident row and `degrade` touches edges, not entity timestamps, so neither is derivable read-only. |
| `ingest` | `tools_mutate.rs` | Add text. `object_id` update semantics, free-text `hint` chunking context (`hint` is the only spelling — the `descriptor` alias retired in `7de23c0`), optional `retention_secs` TTL (integer seconds; `0`/absent = never) resolved to an absolute `valid_until` once, before the sync / durable-direct / RAM-queue branch, so all three carry the same deadline. |
| `link` | `tools_mutate.rs` | Create a reason edge (LLM writes the reason if blank). Edge score is the asserted confidence (agent 0.95; CLI user 1.0), NOT `cosine(from,to)` — a deliberate link connects what similarity cannot, so similarity must not be its strength. |
| `forget` | `tools_mutate.rs` | Remove a thought + cascade edges (Facts immune). |
| `forget_by_source` | `tools_mutate.rs` | Remove every thought from one `(scheme, object_id)` — **all sections of it**, since `source_id` hashes the section and keying on one would forget a single chunk of a document. Cascades through the same `forget_entity`; refuses local Facts unless `force`, which is the ONLY bypass of the Fact guard and is never implicit. Returns `removed_entities`/`removed_edges`/`kept_facts` — the last so a refused Fact is reported rather than read as "nothing was there". Exists so `kern forget --source` has somewhere to route. |
| `degrade` | `tools_mutate.rs` | Down-weight edges along a bad retrieval path (`DEGRADE_*` decay). Returns `decayed_edges` and `removed_edges` — the reap count exists so a CLI `degrade` routed through the daemon can print what the local path prints. |
| `move` | `tools_mutate.rs:467` | Relocate a thought to another kern, carrying outgoing edges and restamping cross-kern references. |
| `promote` | `tools_mutate.rs` | Release a thought a review policy is holding: flips `ReviewState::Pending` to `Active`, so a `query {exclude_pending: true}` returns it again. The release half of the lifecycle `[ingest] review_policy` opens; idempotent, returning `promoted: false` on an already-active row rather than failing, and a hard `thought not found` on an id nothing resolves — a silent success would tell a curator a claim was released while it is still held. Shares `graph_ops::promote_entity` with the CLI's no-daemon fallback so the routed and local writes cannot disagree. Any caller holding the graph's `mcp-token` may promote — the process boundary is the access model. |
| `health` | `tools_admin.rs:83` | Graph stats (gravitons/kerns/entities/reasons/unnamed/claim_kinds) **plus the degradation surface**: `queue_depth`, `tasks_done`, `task_avg_ms`, `task_panics`, `last_task_panic`, `task_failures`, `last_task_failure`, `cold_evicted`, `embed_model`, `embed_dim`, `embed_mismatch`, and the eight fail-open counters — `query_dim_rejected`, `below_floor_deliveries`, `clock_skew_skips`, `ingest_dropped_chunks`, `remote_cap_dropped`, `unspilled_drops`, `ingest_queue_refused`, `gnn_train_refused` — each a path that returns something rather than erroring, so the count is the only way to tell a degraded result from a good one (`Server::health_stats`, `src/mcp.rs:116`). The first seven come off `HealthStats` (`src/health.rs:9`), `gnn_train_refused` straight from the trainer's own global (`src/mcp.rs:149`) — but all eight are process-scoped counters read in the *serving* process, which is why only a daemon's answer carries real ones and any other reader reports its own zeros (`ROADMAP.md` item 100). Beside the counters, one gauge: `ingest_queue_depth` reads the serving worker's mpsc channel occupancy live (`src/mcp.rs:148`, `Worker::queue_depth` at `src/ingest/worker.rs:148`) — how full the RAM queue is right now, where `ingest_queue_refused` only says its bound was ever hit (item 30). |
| `graviton` | `tools_admin.rs` | list/add/remove focus attractors (name + text — phrase or full document — + optional mass). Replaced the single per-kern "purpose". |
| `claim_kind` | `tools_admin.rs` | register/remove claim kinds; registered kinds extend the built-in distill set. |
| `pulse` | `tools_admin.rs` | Trigger a clustering pass across the tree. |
| `gc` | `tools_admin.rs:190-192` | Live reap of empty/orphan kerns (`GraphGnn::gc_empty_kerns_counted`); reports `reaped`/`before`/`after` and the live `data.mdb` size, since LMDB keeps freed pages until a restart or `kern compact`. |
| `intake_drain` | `tools_intake.rs` | One immediate pass of the daemon's own intake drain (`ingest::intake::drain_now`), returning `archived`. Exists so `kern intake drain` has somewhere to route: the CLI's in-process pass reads the same queue directory and archives the same entries as the daemon's poll loop, so both distill the file and both race the archive move. |
| `setup` | `tools_setup.rs` | Agent-facing installer: returns idempotent wiring instructions (seed gravitons, install the capture rule/hook in the host, verify) plus this project's current [done]/[todo] state. kern never writes host config; the calling agent does the wiring. |

Plus MCP **prompts** (`src/mcp/prompt.rs`) and **resources** (`src/mcp/resources.rs`) — four static URIs (`kern://local/health`, `kern://local/thoughts`, `kern://local/kerns`, `kern://local/claim-kinds`, `resource_definitions`) plus two dynamic prefixes resolved in `handle_resource_read`, `thought://{id}` (full text and every incident edge's text) and `reason://{id}` (an edge's text). Anything else is `unknown resource`. **MCP-only: `tests/e2e/` drives the binary over subprocess with no JSON-RPC client, so `resources/read` is unreachable from there**; the coverage is the unit tests in `src/mcp/resources.rs`.

**Server** (`src/mcp.rs`) — `Server` holds the shared `graph`/`worker`/`llm`/
`task_q`/`cfg`; implements `trnsprt::McpServer`. `run`/`run_stdio` use the
trnsprt framing; `run_sse` (`src/mcp/sse.rs`) is bearer-gated Streamable HTTP.

**Where.** `src/mcp/*` (2346 LoC, 9 files).

**Gaps.** Tool schemas are hand-
rolled JSON, not derived. No batch query. **Prompts and resources are served
on both the standalone and the proxy path.** `ProxyServer` (`src/commands_mcp_cmd.rs`)
implements `handle_method` (`:368`): the four graphless methods — `resources/list`,
`prompts/list`, `prompts/get`, `ping` — dispatch through `handle_graphless_method`
(`src/mcp.rs:249`), the same function the standalone `Server::handle_method`
(`:219`) uses, so the two surfaces are one implementation. `resources/read` rides
the `call_tool` passthrough as `RESOURCE_READ_TOOL` (`src/mcp.rs:243`) — transport,
not a tool schema, so `tools/list` is unchanged. `ping` answers `{}` for client
liveness (item 81, closed 2026-07-22).

---

## 13. RPC surface (`kern_rpc`) — `active`

**What.** A `KernRpc` server over a per-root local socket (Unix socket / Windows
named pipe) for local clients that want the daemon without MCP stdio framing.
It is the hub's control channel and the `kern mcp` proxy's data channel. There
is **no tarpc dependency** — the service is generated by this repo's own
`service!` macro (`src/transport/macros/`) over the `typed/` channel + codec.

**How.** The contract is four methods, not a mirror of the tool surface
(`src/transport/kern_rpc/svc.rs`): `health() -> HealthRes`,
`shutdown() -> ShutdownRes`, `call_tool(CallToolReq) -> CallToolRes`,
`list_tools(ListToolsReq) -> ListToolsRes`. Every MCP tool reaches the daemon
through the one `call_tool` passthrough, so the two surfaces cannot drift.
`KernRpcHandler` (`src/rpc/kern_rpc_server.rs:12`) wraps the same `mcp::Server`;
`health` unwraps the `health` tool's JSON envelope into the typed `HealthRes`
(`src/transport/kern_rpc/dto.rs`), which carries the same degradation fields
the MCP JSON does — `task_panics`/`last_task_panic`,
`task_failures`/`last_task_failure`, `cold_evicted`, `embed_model`/`embed_dim`/
`embed_mismatch` — plus `idle_ms` for the hub's idle reaper. Every field is
`#[serde(default)]`, so an older daemon reads as zeros rather than an error.
`shutdown` fires the daemon's save-then-exit path. `serve_kern_rpc_loop`
(`src/rpc/kern_rpc_server.rs`) accepts on a `LocalListener` and spawns a
`Channel` per connection.

**Where.** `src/rpc/*` (613 LoC, 281 before the tests), `src/transport/kern_rpc/`
(`svc.rs` contract, `dto.rs` types, `auth.rs` gate, `client_local.rs` connect).

**Gaps.** The socket has auth now: one token frame carrying the graph's `mcp-token`,
verified before any method dispatches and, since 2026-07-22, bounded by
`AUTH_FRAME_MAX` = 1 KiB (`src/transport/kern_rpc/auth.rs:24`) and `AUTH_DEADLINE`
= 5 s (`:32`), both lifted once the token verifies. The named pipe carries an
owner-only SDDL that typechecks for Windows and has never run on one. Both
directions authenticate: `require_owned_by_caller` (`src/transport/typed/local.rs:237`)
and `require_peer_is_caller` (`:283`) run in `connect_kern` ahead of the token frame
*and* in the bind's `AddrInUse` arm, which refuses a foreign-owned name by uid rather
than standing the daemon down — the peer half pinned only by an injected uid (item 24).

---

## 14. CLI — `active`

**What.** The `kern` binary. Reads the on-disk graph directly (can race a live
daemon — prefer MCP for live state). Ten commands are the exceptions when a
daemon serves: `forget`, `degrade`, `promote`, `intake drain`, `graviton add`,
`graviton remove`, `claim-kind add` and `claim-kind rm` hand it the write, and
`get` and `query` take their read from it.

**Subcommands** (`Commands` enum, `src/commands.rs`): `ingest`, `query`,
`search`, `reembed`, `get`, `list`, `forget [ID | --source <scheme>://<object_id>
[--force]]`, `link`, `intake {status|drain}`,
`status`, `health`, `profile`, `gc`, `compact`, `graviton {add|list|remove}`,
`degrade`, `promote`, `claim-kind {add|rm}`, `peers`, `register`,
`unnamed {list}`, `mcp`, `compress`, `daemon`, `hub {status|resolve|unload|merge|stop}`.

**How.** `dispatch` (`src/commands.rs`) routes; per-subcommand handlers in
`src/commands_{admin,graph_ops,ingest_cmd,intake_cmd,mcp_cmd,mcp_restart,profile_cmd,query,reembed,route,status}.rs`.
Notable:

- **Daemon-first writes** (`src/commands_route.rs`) — `route(name, args)` probes
  `Endpoint::kern()` once, never spawns, and answers `Done` / `Refused` /
  `NoDaemon`. `forget`, `degrade`, `promote`, `graviton add`/`remove` and
  `claim-kind add`/`rm` take it (the last four via `graviton_at`/`claim_kind_at`,
  `src/commands_admin.rs`, which take the endpoint the way `route_to` does so
  the routed path is reachable from a test): while a daemon serves, the
  mutation lands in its live in-memory graph over `call_tool` instead of in a
  second copy this process opened, and a daemon that refuses is reported rather
  than retried against the store behind it. No daemon -> the pre-existing local
  path runs, printing through the same printer so the two cannot drift. The
  graviton add routes before it embeds: the daemon embeds with its own client,
  so a local embed first would spend a model call on a vector nobody keeps.

- **Daemon-first reads** (same route, `query` tool) — `get` (`cmd_get`,
  `graph_ops.rs`) and `query` (`cmd_query`, `query.rs`) route before they touch
  disk, so a serving daemon's live graph answers instead of the older snapshot
  this process would load. `get` routes as `query {id}`, `query` as
  `query {text, mode, k}`. `k` is sent explicitly: the tool's own default is
  `seed_k`, well under the delivery pool the local path prints, so omitting it
  would make the hit count depend on whether a daemon happened to be up —
  `retrieval::score::delivery_cap` is the one owner both sides read it from.
  Both paths render through one printer over the tool's own JSON
  (`print_detail`, `print_results`), and one id resolver serves both
  (`mcp::tools_query::entity_detail_by_id`, prefix-resolving with cold-tier
  fallback), so a routed and a local read cannot disagree about what an id means.
  `search` and `list` stay local **by decision** — `search` is the raw-ANN
  probe with no matching tool, `list` prints the on-disk kern tree, and both are
  what a developer reaches for to inspect the store itself.

- `forget --source <scheme>://<object_id> [--force]` (`graph_ops.rs`) — the
  host-deletion cascade (ROADMAP item 19). Routes to the `forget_by_source` tool
  first for the same reason plain `forget` does, and both branches print through
  one `print_forget_source`. The segment after `://` is the raw
  `Source::object_id()`, not a parsed URI path — that is the half of the pair the
  graph stores, and re-deriving it from a `ticket://<system>/<id>` spelling would
  guess. `--force` is paired to `--source` **in `dispatch`, not by clap**: a
  single id names one Fact the caller can already see, so the bypass only makes
  sense in bulk — and `#[arg(long, requires = "source")]` does not fire for a
  `SetTrue` flag (clap 4.6), which silently accepted and ignored
  `forget --force <id>`. It reaches `remove_entity`'s own fact guard too, not
  just `forget_entity`'s — lifting only the outer one reports a removal the
  inner one silently refused.

- `ingest --retention-secs N` (`ingest_cmd.rs`) — expires the ingest after `N`
  seconds by stamping `valid_until`; `0` or the absent flag means never. The
  deadline is resolved **once, before** the guarded write-retry loop, so a
  refused-stale flush that reloads and re-runs cannot push the expiry out by
  however long the retry took. An overflowing `N` is reported and nothing is
  written.

- **The writer lock** (`src/lock.rs`) — one advisory lock per data dir
  (std `File::try_lock`, MSRV 1.89), held for the daemon's whole lifetime and
  taken by every direct-writer admin command. `reembed`, `compact` and `gc`
  refuse while it is held and name the holder, because "daemon must be stopped"
  was an unenforceable comment: a killed hub is respawned by any surviving
  `kern mcp` proxy, and the respawn flushed its stale graph over a completed
  re-embed. It is an OS file lock, so a killed holder releases it — the file's
  existence is never the lock, and there is no cleanup path.
- **The standalone MCP server takes it too** (`claim_standalone`,
  `src/commands_mcp_cmd.rs`) — `kern mcp`'s no-daemon fallback claims the dir as
  `mcp-standalone` before it reads the graph, and holds it for the process. It
  is the one writer no probe can find: it binds no socket, so a second one is
  invisible to everything except the lock. A failed claim spends one more attach
  window on `Endpoint::kern()` and proxies to whoever answers — normally the
  daemon this process just spawned, late to bind — and exits 1 naming the holder
  if nothing does. A client can lose kern that way; it can no longer get one
  that overwrites another's graph.
- `status` (`status.rs`) — data dir, socket, whether a daemon serves this
  directory, whether the hub runs, and who holds the writer lock. Says so
  explicitly when a daemon serves without holding the lock, since then the
  admin commands will not be refused.

- `reembed` (`reembed.rs`) — re-embeds every entity with a new model in batches,
  re-seeds `gnn_vector` from the raw embed, recomputes reason-edge vectors
  (endpoint means), rebuilds the index, saves, then re-embeds the cold tier. It
  stamps the store with the model it actually embedded with, only after the
  rewrite succeeded; a cold-tier failure is reported explicitly (hot graph on the
  new model, cold tier still on the old). Takes the writer lock and refuses
  rather than racing a live daemon.
- `health` (`admin.rs`) — prints the graph counts, an embed-model mismatch
  warning, `evicted:` and the fail-open `degraded:` line — the daemon's counts
  when one answers, this process's otherwise (item 100) — and, from a daemon,
  `degraded: N panics | M failures | K refused GNN trainings`, faults named
  below, plus `ingest: queue N` — the RAM queue's live depth
  (`ingest_health_lines`, `src/commands_admin.rs:201`), daemon-sourced only
  because the CLI's own worker is idle by construction (item 30). From a daemon
  it also prints `convergence: gini 0.NN` — the Gini coefficient over entity
  access counts (`gini_over_access`, `src/health.rs`, item 62 half),
  `0.0` = uniform access (converged), → `1.0` asymptotically (finite-n max
  `(n−1)/n`); daemon-sourced only because a CLI's fresh-open graph has no
  query history.
- `profile` (`profile_cmd.rs`) — runs a query with a `Profiler` timeline.
- `compress` (`admin.rs`) — compresses vectors with a chosen `QuantizationMode`.
- `daemon` / `run_server` (`src/commands.rs`) — boots the full runtime: loads
  graph, binds the embedding model and checks the store's stamp, spawns
  watchdog, LLM keepalive, file watcher, the intake, gossip, maintenance tick,
  MCP (stdio or SSE), and the RPC socket.
- `kern mcp --embed-url <url> --embed-model <model>` (`EmbedArgs::apply_to`,
  `src/commands.rs`) — per-process override of `[embed]` url/model, applied to
  the loaded config before anything embeds. Exists for spawns (containers)
  whose config cannot name the host's Ollama; absent flags leave `cfg.embed`
  exactly as loaded.

**Where.** `src/commands_*`, `src/lock.rs`, `src/main.rs`.

**Gaps.** `ingest` and `link` still open the store directly while a daemon
holds newer state (`intake drain` routes since 2026-07-21). They deliberately reconcile instead of
refusing — the flush guard rejects a stale write and they reload and retry —
because refusing them would make the CLI unusable whenever a daemon runs.
`ingest` and `link` cannot take the daemon route the way `forget`/`degrade` do,
because the RPC's only mutation surface is `call_tool`, the agent boundary:
`tool_ingest` clamps to `AGENT_SOURCE` and `tool_link` writes
`MAX_AI_CONFIDENCE`, while the CLI mints at user trust 1.0, so routing them
unchanged would demote every CLI Fact to an agent Claim; kern carries no
caller identity by decision (2026-07-22), so the remaining choice — route and
accept the agent clamp, or stay local behind the flush guard — is owed in
`ROADMAP.md` item 9. `get` and `query` no longer read stale:
both route to a serving daemon over the `query` tool and fall back to the disk
load only when nothing answers. `search` and `list` stay local by decision
(`ROADMAP.md` item 9). `kern unnamed promote <id> <name> <seed> [--mass N]`
  promotes an existing unnamed kern to named by giving it a graviton in place
  (no move, no id change — it keeps entities/children/parent and becomes
  `is_named`, so gc keeps it); `accept::promote_unnamed` (`src/accept.rs`).

---

## 15. Federation (gossip + CRDT) — `building`

**What.** Opt-in LAN knowledge sharing with no coordinator. Each node
heartbeats peers and merges entity bodies via content-addressed CRDTs — a
thought ingested on node A becomes searchable on node B under the same id.

**How.**

- **Node** (`src/gossip/node.rs`) — TCP listener, peer list
  (`GOSSIP_MAX_PEERS=50`), `broadcast` with `GOSSIP_FANOUT=3`, `fetch_thought`
  RPC (`GOSSIP_FETCH_TIMEOUT=5s`), `start_heartbeat`
  (`GOSSIP_HEARTBEAT_INTERVAL=30s`), `GOSSIP_MAX_FRAME_BYTES=4MB` bounds. The
  Lamport counter it stamps messages with lives on the graph, not the node
  (`GraphGnn::bump_lamport`/`observe_lamport`, `src/graph.rs:467`/`:474`).
- **Discovery** (`src/gossip/discovery.rs`) — multicast announce/parse on
  `GOSSIP_DISCOVERY_MULTICAST=239.77.75.68` at `gossip.discovery_port`
  (default `7475`, `src/config_gossip.rs:66`) every
  `GOSSIP_DISCOVERY_INTERVAL=10s`. Only pairs nodes sharing the same
  `network_id`.
- **Handler** (`src/gossip/handler.rs`) — `start_announce` (`:110`),
  `start_entity_sync` (`:185`, broadcasts top-32 hottest entities every
  heartbeat), `start_delta_flush` (`:222`, drains `GraphGnn`'s pending CRDT
  deltas), and inbound
  handlers for Sphere/Question/Pulse/PeerExchange/Fetch/CrdtDelta/EntitySync.
- **CRDTs** (`src/crdt.rs`) — `GCounter`, plus the shared `lww_wins` comparison
  the last-writer-wins call sites now route through (`join_lww_time`
  `src/merge.rs`, `merge_reason`, and both `gossip/handler.rs` sites). There
  is no `PnCounter`/`LwwRegister`/`OrSet` type anywhere in the tree. Applied to
  four live `CrdtTarget`s (`src/gossip/types.rs`): `ThoughtAccessCount`
  (GCounter), `ReasonTraversalCount` (GCounter), `ReasonScore` (LWW),
  `ValidUntil` (LWW). This replaces the old wall-clock max-join. `Statements` is
  inert by design — it has no sender, and statements are deliberately never
  imported because entity ids are `content_hash(text)`, so merging them can only
  admit content an id does not hash to.
- **Merge** (`src/merge.rs`) — `merge_entity`/`merge_reason`/
  `merge_remote_entity`/`absorb_graph` apply remote bodies into local kerns
  under `remote-` prefixed ids, capped at `GOSSIP_REMOTE_KERN_ENTITY_CAP=50_000`.
- **Seen / Ledger** (`src/gossip/seen.rs`, `ledger.rs`) — `SeenSet` dedup
  (`GOSSIP_SEEN_SET_CAP=10_000`, TTL 60s); `Ledger` routes thought-id and
  kern-id → peer addr (`LEDGER_THOUGHT_TTL=72h`, `LEDGER_ROUTING_TTL=5min`).

**Status.** Entity-body sharing is verified on a single host with manually
seeded `peers` (the reliable path). Multicast discovery only pairs same-
`network_id` nodes. The **Delta, Pulse and Question senders are all live**
(`src/gossip/handler.rs`, wired from `src/commands.rs`, driven from
`src/tick/tasks.rs`). The **fetch RPC is live**: `wire_fetch`
(`src/gossip/handler.rs:53`) installs the handler at startup
(`src/commands.rs`) and `spawn_fetch_entity` (`src/gossip/handler.rs:74`)
issues fetches from the question path. OR-Set deltas for `statements` are
**dead on both ends by design, not by omission**: `id == content_hash(text)`,
so a same-id peer has identical content by construction and a differing one is
asserting content its id does not hash to. The sender emits empty
(`src/gossip/handler.rs:247`) and the receiver rejects the target
(`src/gossip/handler.rs:502`), kept as a refused variant so an older peer
cannot inject text under a content-addressed id. Statements converge through
full EntitySync bodies. Federation tuning at scale (batch size, push vs pull,
anti-entropy) is open.

**Federation plan v0 (2026-07-27, FEDERATION_PLAN.md §1–§6) — `building`.**
Six layers landed on top of the gossip baseline, each behind its test gate:

- **Identity + signed envelopes** (`src/gossip/identity.rs`) — ed25519 peer
  key minted owner-only at `<data_dir>/peer.key` (`load_or_mint`, same pattern
  as `mint_token`); `PeerId = blake3(pubkey)`; ring location
  `loc = first 8 id bytes / 2^64` (53-bit mantissa, strictly `< 1.0`). Every
  TCP frame travels in a `SignedFrame { pubkey, sig, lamport, body }`
  (`types.rs`); signature covers `blake3(body || lamport_le)`. Verification
  happens inside `transport::decode_msg`, **before** the seen-set, peer list
  or any rate budget — invalid frames are dropped and counted
  (`invalid_sig_dropped`). The Question rate budget now keys on the verified
  PeerId, not the spoofable `origin` string.
- **Small-world ring** (`src/gossip/ring.rs`) — `RingView`: 4 nearest
  neighbors per side + 8 long links harmonically sampled (~1/d, Kleinberg
  exponent 1); greedy `route()` returns only a strictly-closer peer.
  `FindNearest`/`Nearest` frames; iterative client-driven `join_ring`.
  Sim gates: 1k peers, 99% of random targets reach the nearest peer in
  ≤ log²n hops; 20% churn repaired from far links, routing still terminates.
  Off by default (`gossip.ring = false`).
- **Contracts** (`src/gossip/contract.rs`) — the key IS the policy:
  `ContractId = blake3(kind_tag || bincode(ParamsV0))`; `ParamsV0` = owners,
  `WritePolicy` (Open/Allowlist/OwnersOnly), kind filter, `max_entities`,
  forced `retention_secs`, optional `PrivacyV0`. Builtin
  `SignedCrdt`: `validate_delta` refuses forged ids (`id_matches_body`), bad
  or inadmissible writer signatures, off-kind entities and cap breaches —
  counted (`contract_refused`), never panicked; `apply` is
  `merge_remote_entity` behind the trait (commutative + idempotent, proven by
  property tests); `summarize`/`diff` = sorted `(id, lamport)` pairs in 16
  nibble buckets — divergent states converge byte-identical after one
  exchange each way. Contract kerns are `remote-contract-<hex>`, so every
  existing `remote-` trust boundary applies.
- **Subscription trees + delta sync** (`src/gossip/subs.rs`, `handler.rs`) —
  bounded LRU `SubTable` (cap 256): `Subscribe` routes toward
  `loc(ContractId)`, each hosting hop records the asker downstream, answers
  `SubAck{summary}`, extends the tree once toward the key. First parent wins
  (a raced SubAck cannot re-root a node) and a downstream peer is never
  adopted as parent — both guards close the cycle that starved the tree in
  fuzzing. Live deltas (`ContractDelta`) forward only on `Applied::changed`;
  anti-entropy (`SyncSummary`/`SyncDiff`, every `gossip.sync_interval_secs`,
  default 300) reconciles with tree parents. Gate: three real-socket nodes
  chained; leaf publish reaches all three; a healed partition converges in
  one sync pass.
- **Delegates** (`src/mcp/tools_delegate.rs`) — the daemon is the delegate:
  mcp-token-gated `sign { payload_hash }` returns signature/pubkey/peer-id
  (the key never crosses the socket); `contract_grant { contract, pubkey }`
  mints the owner-signed amended params, the NEW ContractId (key = policy
  hash, so grants move the key) and the tombstone signature subscribers
  verify (`tombstone_digest`).
- **Privacy** (`src/gossip/privacy.rs`) — `PrivacyV0` scheme 0 =
  xchacha20poly1305. `encrypt_entity` seals text client-side; the id becomes
  the ciphertext's content hash so `id_matches_body` and writer signatures
  keep holding on relays; vectors are dropped (an embedding of hidden text
  would leak it). Gate: a relay's whole serialized kern never contains the
  plaintext sentinel; only a key holder decrypts. Symmetric key = owner-only
  file (v0 out-of-band distribution).

**Security.** Legacy `network_id` mode remains **unauthenticated** in trust
terms (Open implicit contract) but every frame is now envelope-signed and
invalid signatures are dropped pre-state. Contract kerns are authenticated
end-to-end: per-entity writer signatures + per-frame envelopes. Off by
default. Full trust model on the docs-site `Security` page
(`docs/site/content/docs/concepts/security.mdx`).

**Where.** `src/gossip/*` (13 files), `src/crdt.rs`, `src/merge.rs`,
`src/mcp/tools_delegate.rs`, config in `src/config_gossip.rs`
(`ring`, `identity_path`, `sync_interval_secs`, `subscriptions`,
`[[gossip.contracts]]`).

**Gaps.** Legacy EntitySync still ships only the hottest 32 by heat per
heartbeat (contract kerns get real anti-entropy; legacy kerns do not). No
backpressure on remote-id cap (drops new, keeps known). The `Delta`
(CRDT-counter) path has no per-peer budget — but rotating `origin` no longer
buys a fresh Question budget: that budget keys on the envelope-verified
PeerId, so evasion now costs a keypair per budget. No divergence signal in
`HealthStats` (`ROADMAP.md` — "Backpressure, divergence metric, and delta
write-lock starvation"). Contract signed-body envelopes (`ContractState`)
live in memory only — after a restart a node re-serves what peers push or
re-publishes what it authored, but cannot prove foreign bodies it merged
before the restart until a peer re-serves them; persisting the envelope
cache is open. Ingest-side routing of local claims INTO a shared kern is a
publish seam (`publish_to_contract`), not yet a `[ingest]`-config path.
Wasm contracts stay a reserved seam (extism, `plugins` feature) — not in v0.
The unauthenticated local-row reach stays closed: LWW deltas only touch
`remote-*` kerns (`remote_kern_ids`), `handle_pulse` rejects an unknown kern
id and clamps the deposit, and a body whose text does not hash to its
claimed id is dropped on receipt (`id_matches_body`).

---

## 16. LLM client — `active`

**What.** One client wrapping two endpoints (reason / embed) against Ollama by
default; fail-open everywhere. A non-local configured URL warns at config load
(item 78, 2026-07-22) — `is_local_url` (`src/llm.rs`) detects loopback/RFC1918/
link-local/`ollama`/`` `:11434` ``, `Config::egress_warnings` returns one warning
per non-local URL, and `boot_config` emits each via `tracing::warn!` (non-fatal).

**How.** `Client` (`src/llm.rs:117`) — `embed` (`:220`) / `embed_batch` (`:264`)
against the embedding endpoint, `complete` (`:320`, reason / distillation),
`complete_func` (`:388`, sync closure for the tick/ingest blocking bridges).
`is_transient` (`:21`) classifies retryable errors — on both legs now: the completion leg counts and names what it throws away (`record_complete_failure`, `:74`, bounded to one line by `:65`), so `complete_func`'s `""` no longer hides which failure produced it. It reads back as `llm_complete_failed` / `last_llm_complete_failure` (`src/mcp.rs:150`, `src/commands_admin.rs:201`). **Every request is bounded** — `complete` posts under `[reason] timeout_secs` (`src/config_reason.rs:12`, default 600 at `:20`), applied by `with_timeout_secs` (`src/llm.rs:202`) and held as `reason_timeout` (`:137`, posted at `:344` / `:371`); `EMBED_TIMEOUT` = 120s on the embed calls (`:494`), applied per request by `post_checked` (`:243`) over a client-wide 120s default and a 3s `connect_timeout` (`:159`, `:162`) so a dead endpoint fails fast instead of hanging. `Endpoint` (`:100`) holds
url/model/key; `new_embed_only` (`:213`) builds a client for `reembed`.
`for_eval(seed)` (`:184`) makes it deterministic.

**Where.** `src/llm.rs` (852 LoC).

**Gaps.** Ollama-centric; OpenAI-compatible only via manual url/key. No
retry/backoff policy object. The embedding dimension still locks the graph and
`reembed` is the only escape — what exists now is *detection*, not prevention:
the store stamps the model and dimension, `health` reports a mismatch, and the
query path refuses off-dimension vectors (see §7), but nothing validates the
configured model against the store before the first embed of a session.

---

## 17. Profiling — `active`

**What.** Lightweight per-phase timing for queries and the tick.

**How.** `Profiler` (`src/profile.rs:16`) records labeled `Checkpoint`s
(`:4`) with `Instant`; `finish` (`:35`) produces a `Profile`; `render_timeline`
(`:73`) draws an ASCII Gantt. Used by `retrieve_profiled` and the `profile` CLI.

**Where.** `src/profile.rs` (262 LoC).

---

## 18. Transport layer (`crate::transport` module) — `active`

**What.** MCP JSON-RPC framing plus a typed local-RPC toolkit, inlined as a
`kern` lib module (`crate::transport`). Its whole public surface is what `src/transport/mod.rs`
re-exports: `McpError`, `serve_http`, `serve_rw`, `serve_stdio`, `McpServer`,
`ToolResult`, `ToolSchema`, `PROTOCOL_VERSION`, the `service!` macro, and the
`typed`/`hub_rpc`/`kern_rpc` modules.

**How.**

- **Server** — the `McpServer` trait (`src/transport/mcp.rs:123`:
  `tools_list` and `call_tool` required; `server_name`/`server_version`/
  `extra_capabilities`/`handle_method` defaulted) and `serve_stdio`
  (`src/transport/mcp.rs:142`) / `serve_rw` (`:152`), JSON-RPC over any
  reader/writer. `PROTOCOL_VERSION = "2024-11-05"` (`src/transport/lib.rs:11`).
- **HTTP** — `serve_http` (`src/transport/http.rs:45`, axum), optional bearer
  token.
- **Typed** (`src/transport/typed/`) — the local-RPC substrate: `Adapter`
  (`src/transport/typed/adapter.rs:10`, plus an in-process pair),
  `Codec`/`JsonEnvelopeCodec` (`src/transport/typed/codec.rs:7`/`:18`),
  `Channel` (`src/transport/typed/channel.rs:8`), and
  `src/transport/typed/local.rs` — `Endpoint`
  (`kern()`/`kern_for(root)`/`hub()`), `bind_kern_listener` (`:549`) /
  `connect_kern` (`:314`), `LocalListener` (`:602`), and the two platform
  adapters (`UnixStreamAdapter`, `NamedPipeAdapter`). `require_owned_by_caller`
  (`:237`) and `require_peer_is_caller` (`:283`) guard both ends — `connect_kern`
  and `bind_unix`'s `AddrInUse` arm (`:513-540`), which returns
  `BindError::Untrusted` naming the foreign uid, and unlinks nothing it refused.
- **Service macro** (`src/transport/macros/`) — `service!` turns a trait of
  `async fn`s into client + server + dispatch code. Both RPC contracts are one
  short file each: `kern_rpc/svc.rs`, `hub_rpc/svc.rs`, with their DTOs beside
  them.

**Where.** `src/transport/` (workspace member, 2298 LoC across 20 files including
the macro crate).

**Gaps.** No connection pooling in the local clients — each `connect_*` opens a
fresh socket. There is no MCP *client* and no multi-server registry: kern is
always the server here.

---

## 18b. Lifecycle freshness — auto-restart + hot reload — `active`

**What.** Two mechanisms that keep a long-lived daemon from serving stale code
or stale config indefinitely (the 36h dead-endpoint dogfooding outage,
2026-07-21).

**How.**

- **Identity** (`src/identity.rs`) — `build_id` = sha256 of the
  executable's `(len, mtime)` fingerprint (path excluded: `cargo install`
  hardlinks `target/release`; semver excluded: every dev build reports the
  same version), `config_id` = sha256 of the serialized resolved config,
  `uptime_ms` stamped at bootstrap. All three ride `HealthRes` (append-only,
  empty/0 from older daemons).
- **Client-side auto-restart** (`src/commands_mcp_restart.rs`, applied in
  `mcp_cmd.rs` `replace_if_stale`) — on attach, `kern mcp` compares identities.
  Verdict is a pure tested function: `Fresh` proxies; `Stale` (differs AND
  daemon uptime ≥ 15s) triggers graceful `shutdown` → socket-release wait →
  respawn → reattach; `Hold` (young daemon, empty ids, or unreadable self)
  warns and proxies — unknown is never stale, and the 15s floor stops two
  differing builds restarting each other in a loop. `[hub] auto_restart`
  (default true) gates the restart, never the warning. Fail open at every
  step: an unreachable health, failed shutdown, or failed respawn falls back
  to proxying.
- **Hot reload** (`src/takeover.rs`, Unix only, `[reload] enabled` default
  true, `poll_secs` default 3) — the daemon polls its own binary path
  (deleted-marker-stripped); a changed fingerprint must survive two
  consecutive polls (torn mid-link file never fires). Trigger reuses the
  graceful shutdown path (drain, guarded flush), then spawns the successor
  with the listening socket dup'd in as fd 0 (`Stdio::from(OwnedFd)` — dup2
  clears CLOEXEC, no libc dep) with `KERN_TAKEOVER=1`, and `process::exit(0)`s
  — deliberately skipping `LocalListener`'s Drop, which would unlink the
  socket path under the successor's fd. The successor adopts fd 0
  (`trnsprt adopt_kern_listener`), skips bind, AlreadyRunning probe (would
  eat a queued connect) and store self-heal (predecessor still holds the env
  for ms). Connects during successor boot queue in the kernel backlog; the
  MCP proxy reconnects severed connections and retries the call once
  (idempotent: ingest is content-addressed, queries are reads). Measured
  handover on the dogfood store: 39ms listener gap, zero refused connects.
- **Windows** — no fd handoff for named pipes; client-side auto-restart is
  the coverage there.

**Gaps.**

- Hub-tracked nodes: after a takeover the hub's `NodeHandle` holds the dead
  predecessor's PID; the reaper drops it and the next resolve re-adopts via
  probe — eventual, not immediate.
- The queued-job loss window on reload equals the existing Ctrl-C graceful
  path (RAM-only fallback enqueues); durable intake files survive by design.

## 18a. Hub — machine-level control plane — `active`

**What.** `kern hub` is a per-machine supervisor: one socket (`kern-hub.sock`),
a routing table of project root → node daemon. Clients resolve a root through
the hub; the hub spawns the node if absent (or adopts an externally started
daemon), unloads it gracefully on request, auto-unloads idle nodes, and merges
one project's store into another offline. The data path stays direct
client→node — the hub is connect-time only, never a proxy hop.

**How.**

- **hub_rpc** (`src/transport/hub_rpc/`) — a four-method service
  (`svc.rs`): `resolve(ResolveReq)`, `status()`, `unload(UnloadReq)`, `stop()`,
  plus a `connect_hub` client (`client.rs:11`). `Endpoint::hub()`
  (machine-scoped), `Endpoint::kern_for(root)` (hub computes a node's socket
  without chdir).
- **Supervisor** (`src/hub/`, 547 LoC) — `node.rs` spawn/probe/ready-wait/
  shutdown, `serve.rs` handler + accept loop + dead-node reaper (`run_hub` at
  `src/hub/serve.rs:294`). Hub exit leaves nodes running; a restarted hub
  re-adopts them via probe. `canon` re-pins any path to the nearest `.kern`
  ancestor, so two clients in different subdirs resolve to one node.
- **Graceful unload** — `KernRpc::shutdown` fires the daemon's save-then-exit
  path (no signals, works on Windows named pipes too).
- **Idle auto-unload** — nodes report `HealthRes.idle_ms` (last real tool call,
  health polls excluded); the hub reaper re-checks under the per-root lock and
  unloads hub-owned nodes past `--idle-unload-secs` (default 1800, 0 off).
  Adopted nodes are exempt; `idle_ms == 0` (pre-field daemon) is never trusted.
- **Cross-kern merge** — `kern hub merge <src> <dst>`: stops both daemons,
  offline CRDT union via `base::merge::absorb_graph`, src never written.
- **Hub-first proxy + auto-start** (`src/commands_mcp_cmd.rs`) — `kern mcp` asks
  the hub first, auto-starting a detached hub when none answers
  (`[hub] auto_start = false` opts out); any failure falls through to the
  direct-connect/auto-spawn fallback. `kern hub stop` ends the hub over
  RPC; nodes stay up.
- **Detached children are logged.** Both spawners — the hub
  (`spawn_hub`/`spawn_daemon`, `src/commands_mcp_cmd.rs`) and the hub's per-root
  node (`src/hub/node.rs:104`) — route the child's stdout *and* stderr into an
  append-only, owner-only file under `Config::log_dir()` = `<data_dir>/logs`
  (`src/config.rs`), one file per spawn arg: `hub.log`, `daemon.log`
  (`detached_log::log_path`, `src/config_detached_log.rs:10`). Append, never
  truncate — a restart must not erase the log explaining why it restarted. A log
  that cannot be opened falls back to `/dev/null` and says so on the parent's
  still-attached stderr, so an unwritable log never costs the spawn.

**Where.** `src/hub/`, `src/transport/hub_rpc/`, `src/commands_admin.rs`
(`cmd_hub`), `src/config_hub.rs`, `src/config_detached_log.rs`,
`tests/e2e/test_hub.py`.

**Gaps.** Gossip still lives in each node; the transport moves hub-side
together with the TLS work (ordering recorded in `ROADMAP.md` — "Hub phase 3:
gossip moves hub-side"). Version skew
hub↔node unmanaged beyond same-binary spawning.

---

## 19. File watcher (`watcher` crate) — `active`, off by default

**What.** Watches repo roots and turns file events into ingest records.
**Opt-in** (recorded 2026-07-21 — this section was marked plain `active` and
never said so): `WatcherConfig::enabled` is a `bool` behind `#[derive(Default)]`,
so it is `false` unless a `kern.toml` sets it, and `effective_roots` returns an
empty list while it is (`src/config_watcher.rs:8`, returned `:24-26`). Everything below runs
only in a deployment that turned it on — which is what ranks its gaps, the same
way `Federation` says "off by default" rather than leaving it to be inferred.

**How.** `FileWatcher` (`src/watcher/file.rs`) wraps `notify`, emits
`WatchEvent`s (`event.rs`: `Created`/`Modified`/`Deleted`/`Renamed {from, to}`).
`IgnoreRules` (`ignore_rules.rs:5`, built `from_roots` over ripgrep's `ignore`
crate — a real `Gitignore` per root for `.gitignore` and `.kernignore`, plus
host-supplied `with_denied` prefixes (`:45`) that no ignore file can unset)
filters noise. `IngestPipeline` (`pipeline.rs:24`) debounces, caps at
`MAX_INGEST_BYTES=1MB` (`pipeline.rs:7`), and pushes `IngestRecord`s to an
`IngestSink` (kern's is `KernFileWatcherSink`).

**Where.** `src/watcher/` (a `kern` lib module, not a separate crate; inlined 2026-08-06).

**Gaps.** *Both claims here were stale and are corrected 2026-07-21.* `.gitignore`
parsing is **not** approximate — `IgnoreRules` builds a real `Gitignore` through
ripgrep's `ignore` crate (`src/watcher/ignore_rules.rs:3`, matched `:71`), so
it is the full spec; the deliberate deviations are the unconditional `.git` skip
(`:60`) and the host's denied prefixes (`:63`), which no ignore file can unset. Renames **are** tracked at the event layer — `WatchKind::Renamed {from, to}` (`src/watcher/event.rs:9`) carries both
endpoints. What is actually missing is graph-level re-keying: `build_record`
ingests `to` and discards `from` (`src/watcher/pipeline.rs:48`), so a rename
lands as a new `Document` and the old one is neither moved nor removed.

---

## 20. Config — `active`

**What.** Layered TOML config, all-optional (works zero-config against local
Ollama). The whole memory-tuning surface is one key: `preset = "relaxed" |
"medium" | "tight"`.

**How.** `Config` (`src/config.rs`) aggregates sub-configs — `Embed`,
`Reason`, `Serve`, `Retrieval`, `Ingest`, `Gossip`, `Tick`, `Heat`,
`Gnn`, `Watcher`, `Intake`, `Graph`, `Hub` — plus `data_dir`, `preset`, and a
derived `log_dir()` = `<data_dir>/logs`. Resolved project-scope
(`<cwd>/.kern/kern.toml`) over user-scope (`<XDG_CONFIG>/kern/kern.toml`).
`Config::resolve_root` walks up to the nearest `.kern/` ancestor. Under WSL2 NAT
a loopback Ollama URL must be pinned to the Windows host gateway in `kern.toml`
— kern does not rewrite URLs.

**Presets own the tuning knobs.** `Preset::apply` (`src/config_preset.rs`) is
the only writer of heat half-life, ingest dedup threshold, and retrieval
breadth (`seed_k`, `max_expansions`, `max_deliver_results`). Default is
`relaxed`: 30d half-life, 0.98 dedup, seed_k 25 / 800 expansions / 40 results.
`medium` = the neutral sub-config struct defaults (7d, 0.95, 15/500/25, pinned
together by test); `tight` = 3d, 0.90, 10/250/12. The `[heat]`, `[ingest]`,
and `[retrieval]` sections are **refused** at load with a pointer to `preset`,
and `[answer]` is refused with a removal notice (2026-07-21: kern does no
synthesis; the calling agent does) — no silently ignored keys. Project-scope preset beats user-scope preset like any other key.

**Scopes deep-merge, per key.** `merged_value` → `merge_deep`
(`src/config_io.rs`) recurses wherever both scopes hold a table, so a
project setting one field of a section keeps every other field the user set in
it. Arrays and scalars are **leaves**: `over` replaces, never appends —
`watcher.roots` and `gossip.peers` are complete lists, not accumulators. Both
files are parsed as documents (`toml::Table`), because a bare-`Value` parse
misreads a leading `[section]` header as an array.

**One exception, deliberate: a redirected endpoint does not inherit its key.**
`secrets::seal_redirected` (`src/config_secrets.rs:15`) strips `key` from any
section where the project scope set `url` and did *not* set `key`. Without it a
cloned repo committing `[embed] url = "http://attacker.example/v1"` would harvest
the user's live key on the first embed call — and `reason_key` falls
back to `embed.key`, so redirecting any one endpoint reaches it. A project that
leaves `url` alone keeps inheriting the key, which is the whole point of
layering.

**A bad config aborts startup.** `boot_config` (`src/main.rs:16`) treats every
error `Config::load` returns as fatal: unreadable or unparseable file, or a
`Config::validate` failure. It prints the offending key on stderr and exits
`78` (`EXIT_CONFIG`, sysexits(3) `EX_CONFIG`), which distinguishes "your settings
are wrong" from a crash. An **absent** config is still legitimate and defaults
silently — `load` already handles `NotFound` — so every error it does return is
a real one. The CLI is parsed *first*, so `--help`/`--version` still answer in a
repo whose config is broken.

**Per-endpoint Ollama-native knobs.** `[embed]` and `[reason]` each take
`num_ctx` (u64, 0 keeps the default — 2048 embed / 8192 reason) and `keep_alive`
(string, empty keeps the default — `10m` embed / `2m` reason). These were
constants in `src/llm.rs`; they are now config so a model with a larger context
or a different residency can be tuned without a recompile. They are sent only on
the Ollama-native path (`wants_native`, `src/llm.rs`) — a `/v1` (OpenAI-compat)
endpoint has no client-side `num_ctx` or `keep_alive`, so `Config::native_knob_warnings`
(`src/config.rs`) emits one non-fatal `tracing::warn!` per knob a config sets
on a `/v1` endpoint, at boot, alongside the `egress_warnings` from item 78. Default
knobs on a `/v1` endpoint are silent — a default is not "trying to tune", so there
is nothing to warn about.

**Where.** `src/config_*` (17 files), `src/main.rs` (boot gate).

**Gaps.** No env-var override layer. Secrets (API keys) stored in plaintext TOML.
`validate` covers embed url/model and delegates to the sub-validators; sections
with no validator can still hold nonsense that only fails at use. Preset tier
values are hand-picked, not eval-measured — the e2e instrument has only ever
scored the medium-era defaults, and the shipped default is now `relaxed`
(ROADMAP item 87).

---

## 21. Bench & eval — `removed 2026-07-20`

The LoCoMo end-to-end eval, the retrieval bench, both feature-gated binaries
and the `bench` feature are deleted. They measured
`ingest x retrieval x answering` as one LLM-judged number, which is dominated
by the answerer: a grounded run (whole conversation in the prompt, kern
bypassed) scored 0.187, so answer quality — not memory — set the ceiling, and
three prompt tweaks moved the score more than any retrieval change.

What replaced it is `21a` below: `tests/e2e/` scores retrieval over a corpus the test
writes itself, so no answerer and no judge sit in the loop. The constraint that
sank every id-mapping proposal — ingest records no claim→source-turn mapping, so
turn-level claim provenance does not exist — is sidestepped rather than solved:
a test that ingests the facts already knows which id is correct.

## 21a. E2E harness (`tests/e2e/`, Python) — `active`

**What.** `just e2e` (pytest) drives the real `kern` binary end to end, and is
**the instrument retrieval quality is measured with** (`ROADMAP.md` item 1):
retrieval ranking, the hub supervisor lifecycle, VISION-criterion invariants, and
a scored recall metric.

**How.** `fake_llm.py` serves the native Ollama API deterministically —
`/api/embed` returns feature-hashed bag-of-words vectors (token overlap gives
real cosine ranking, no GPU or model), `/api/chat` echoes the last user
message so a test can assert what reached any chat-completion prompt in the
prompt. `conftest.py` isolates each test in a private project (own
`XDG_RUNTIME_DIR`, `XDG_CONFIG_HOME`, `.kern/kern.toml` pinned to the fake).
`test_hub.py` is the ported Rust hub supervisor suite.

**Measured.** `tests/e2e/test_recall.py` — 36 facts, 72 paraphrase probes, scored
`recall@1` / `recall@5` / `MRR` against floors, printed on every run (`-s`).
Current: **0.9306 / 0.9722 / 0.9471** (2026-07-21, after item 86's traversal
credit; the founding 0.9583 / 1.0000 / 0.9792 predates the answer-leg removal),
bit-identical across runs because the fake embedder has no RNG and no clock. `tests/e2e/test_invariants.py` asserts the properties
each `VISION.md` criterion promises — self-recall, content addressing, supersede
ordering, degrade, Fact durability.

**Measured with the GNN running.** `tests/e2e/test_gnn_recall.py` — the same 36 facts
and 72 probes, but scored on a graph a real propagation has touched: it lowers
`[gnn] min_thoughts` to 4 (e2e-only; the shipped floor is 128), pins `[tick]
interval_secs = 0` so only the daemon's boot pass runs, and **refuses to score
until the daemon's own `learned propagation applied` line arrives** naming at
least 30 nodes. Floors **0.85 / 0.93 / 0.88**, set below the worst of 8 runs
(0.8889–0.9306 / 0.9583–0.9722 / 0.9219–0.9508) because propagation is
stochastic — not comparable to the CLI corpus's floors, since the seed index is
fused 0.6/0.4 with the propagated one.

**Where.** `tests/e2e/conftest.py`, `tests/e2e/fake_llm.py`, `tests/e2e/ranking.py`,
`tests/e2e/test_retrieval.py`, `tests/e2e/test_invariants.py`, `tests/e2e/test_recall.py`,
`tests/e2e/test_gnn_recall.py`, `tests/e2e/test_hub.py`, `tests/e2e/requirements.txt`; `justfile`
recipes `e2e` and `e2e-install`; `.github/workflows/ci.yml` job `e2e`.

**Gaps.** The floors make this a **regression detector, not a quality claim** —
it can say kern got worse, never that kern is good, and no number here is
comparable to anything a competitor publishes. The fake embedder is bag-of-words
hashing, so it measures kern's machinery (fusion, expansion, ranking, dedup,
supersede, heat) and nothing about a real embedding model's semantics. Four
invariants cannot be asserted at all and stand as `skip` markers naming the
missing surface: `supersede` and `as_of` are unreachable from the CLI (MCP
only), path-scoped `degrade` is inexpressible (`kern degrade` takes one entity
and decays every edge incident on it), and "an ordinary thought is evictable"
has no CLI construction because everything the CLI ingests comes back
`Kind: Fact`. No `xfail` remains: the reason-edge invariant is a hard regression
test since item 86 closed, as is the former query-ranking one (hybrid fusion
rescores seeds by query cosine; see CHANGELOG 2026-07-20). Windows: hub tests
skip (unix sockets); retrieval tests unverified there.

## 21b. Docs site (`docs/site/`, fumadocs) — `active`

**What.** The published documentation at yesitsfebreeze.github.io/kern —
25 pages built with fumadocs (Next.js, static export), in three sections:
**Concepts** (the mental model, including `security` — the whole trust model:
local socket and MCP surface, plaintext-at-rest, LLM egress, and the
federation CAN/CANNOT tables), **Decisions** (per-mechanism design rationale
ported from `docs/kern/` research notes and re-verified against source), and
**How-to** (task-shaped guides).

**How.** MDX content in `docs/site/content/docs/`; `next build` with
`output: 'export'` emits `docs/site/out/`. Client-side Orama search from a
statically cached index (`/api/search`), mermaid rendered client-side,
`/llms.txt` and `/llms-full.txt` generated from the page tree for LLM
consumption. `NEXT_PUBLIC_BASE_PATH=/kern` in CI for GitHub Pages;
`.github/workflows/docs.yml` builds on docs changes and pushes `out/` to the
`gh-pages` branch. Replaced mkdocs + terminal theme + custom TUI overlay
(deleted 2026-07-20).

**Doc/code contract.** Pages cite exact `src/…:line` locations, so drift is
mechanically checkable: `tests/docs_check.py` fails on any citation naming a
missing file or a line past EOF, any backticked repo path under
`docs/`/`tests/`/`.github/`/`.pi/` that does not exist, any relative
`.md`/`.mdx` page link whose target does not exist, and any link into this
repo's own files on GitHub that names a file not committed — the check that
would have caught the month-long dead `install.sh` link. It scans every
documentation directory: `docs/site/content/`, `docs/kern/`, `docs/oracle/` and
`README.md`. Two escapes carry the citations that are *meant* to name something
gone — a page holding `<!-- docs-check: historical -->` is skipped whole
(`CHANGELOG.md`), and a line naming a deletion is excused in place, so a
present-tense page can still record what it removed. `--selftest` pins the
regexes and the escapes.
`.github/workflows/docs-check.yml` runs it on every push and PR, deliberately
unfiltered by path. Pages state only what exists today (including honest "not
built"); what is *left* lives solely in `ROADMAP.md` per repo law 4.

**Where.** `docs/site/` (app + content), `tests/docs_check.py`, `justfile`
recipes `docs` (dev server), `docs-build`, `docs-install`, `docs-check`.

**Gaps.** No custom theme — stock fumadocs UI by explicit choice. Local dev
needs `npm ci` in `docs/site` once. `docs_check.py` proves a cited line
exists, not that it still holds the claimed thing — semantic drift is caught
only by audit.

## 21c. CI and repo bootstrap — `active`

**What.** What a push has to survive, and what a fresh checkout needs to run.

**CI** (`.github/workflows/ci.yml`) — five jobs:

- **lint** — runs `just check`, which is `cargo fmt --all -- --check` plus
  `cargo clippy --all-targets -- -D warnings` (`justfile:13-15`). CI invokes the
  recipe rather than copies of its command lines, so the local bar and the CI
  bar cannot drift.
- **e2e** — `just e2e-install` then `just e2e` (pytest) on Linux only: the hub
  module skips wholesale on win32 (unix sockets), so a Windows e2e job would
  report green on nothing. `conftest.py` builds the binary itself; the job also
  builds first to warm the cache and keep a compile failure out of the pytest
  report.
- **test** — `cargo build`/`cargo test --workspace --locked` on Linux, macOS and
  Windows runners (tests actually execute).
- **build** — cross-compiles the `kern` binary for 15 targets, build-only.
- **vocab** — bans the scrubbed synonym for the intake. It now *works*: the old
  form branched on `grep`'s exit code, and GNU grep returns 2 for a missing path
  **even when it matched**, so with a gitignored path in the list the step could
  never fail. It tests the captured output instead.

Three more workflows: `.github/workflows/docs-check.yml` (runs `docs_check.py`
on every push and PR, deliberately unfiltered by path),
`.github/workflows/docs.yml` (builds and publishes the site), and
`.github/workflows/release.yml` (on a `v*` tag or manual dispatch: the same 15
targets, built `--release --locked`, packaged per-target and attached to the
GitHub Release the install scripts fetch from).

**Bootstrap** — `.pi/update.sh` is **tracked**. It was previously matched by the
default-deny `.gitignore`, so the file existed locally and in no clone: the
fresh-checkout guarantee it describes did not exist for anyone else. It runs
`just docs-install` and `just e2e-install`.

**Gaps.** The lint job is the only gate on formatting, so a change that only
touches non-Rust files can still land unformatted docs. Cross-compiled targets
are built, never run.

## 22. Cross-cutting utilities

- **math** (`src/math.rs`) — `cosine`, `cosine_distance`, `l2_normalize`,
  `average_vec`, content-hash `reason_id`, `OnlineSoftmax`, `softmax_merge_scores`,
  `clamp_confidence` (caps AI confidence at `MAX_AI_CONFIDENCE=0.95`, Facts at 1.0).
- **util** (`src/util.rs`) — `content_hash`, `now_nanos`, `cmp_rank`
  (deterministic tiebreak on score then id), token estimation.
- **time** (`src/time.rs`) — clock helpers (graceful on unreadable clock).
- **health** (`src/health.rs`) — `graph_health_stats`: graph counts plus the
  store signals (`cold_evicted`, `embed_model`, `embed_dim`, `embed_mismatch`)
  and `query_dim_rejected`. Storeless graphs report zeros, and an unstamped store
  falls back to the dimension the graph actually holds — unknown is never
  reported as a mismatch.
- **log throttle** (`src/log_throttle.rs`) — `LogThrottle`, the one-line-
  per-interval guard behind the embed-mismatch, dimension-guard and cold-eviction
  warnings. A degradation that repeats per row must not become the log.
- **constants** (`src/base_constants.rs`) — every magic number in one file.
  The 7 built-in claim kinds are **not** here, and there is no claim-kinds
  module under `src/`: they are the `DEFAULT_KINDS` const in
  `src/ingest/distill.rs:9`.
- **test support** (`src/test_support.rs`) — `cfg(test)` graph/entity/edge
  builders shared across the unit tests. There is no `src/log/` or
  `src/test-utils/` crate; the workspace members are exactly `src/transport`
  and `src/transport/macros` (`Cargo.toml:3`). `src/watcher` is now a `kern` lib
  module, inlined 2026-08-06.

---

## 23. Improvement opportunities (consolidated)

Ranked by leverage:

1. (retired 2026-07-21 — ROADMAP item 86 closed) a reason edge does lift its
   neighbour now: bounded source-weighted traversal credit in `expand`, clamped
   below the strongest voucher, all 8 linked pairs in the top 5.
2. **O(N) importance scan per retrieve** (`src/retrieval/seed.rs:131`) —
   `seed_important` walks every entity each query (`par_iter`, `:143`); index
   it, it's the scaling cliff at query time. Open as `ROADMAP.md` item 25.
3. **Federation security** — add auth + encryption before any real deployment
   (`ROADMAP.md` — "Transport security"). The three fixes that needed no auth
   are done: LWW deltas confined to `remote-*`, pulses id-checked and clamped,
   remote bodies hash-verified against their claimed ids.
   Trust model: `docs/site/content/docs/concepts/security.mdx`.
4. **Nothing bounds memory deterministically** — corrected 2026-07-21, this
   entry named the wrong knob. `KERN_CAP_DISABLED` (`src/base_constants.rs:30`)
   is a *kern-eviction* sentinel, not a per-kern entity cap: it defaults both
   `max_loaded_kerns` (`enforce_kern_cap`, `src/graph.rs:227`) and
   `disk_threshold` (spill trigger, `:296`) to `usize::MAX`, so neither eviction
   nor DiskANN spill is armed. A per-kern entity cap for local kerns does not
   exist at all. A safe cap + escalation policy is still the wanted fix.
5. **CLI vs daemon race, serving half** — the destructive half is closed:
   `src/lock.rs` is an advisory writer lock and `reembed`/`compact`/`gc`
   refuse while a daemon holds it, with `kern status` reporting the holder. The
   route decided for the rest exists (`src/commands_route.rs`) and `forget`,
   `degrade`, `graviton add`/`remove` and `claim-kind add`/`rm` take it — the
   last four closed 2026-07-21, and they were the ones that mattered most: with
   no routing at all they reached `with_graph`, which writes the whole kern map
   back unguarded over whatever the daemon had committed. `kern mcp`'s
   standalone fallback — the last long-lived
   second writer, and one no probe can see — now claims the same lock before it
   reads the graph and refuses to boot beside a holder (`claim_standalone`,
   `src/commands_mcp_cmd.rs`). The read side is done: `get` and `query` route
   through the same `query` tool and print through one printer, with the local
   load as the `NoDaemon` fallback; `search` and `list` stay local by decision.
   `kern link` no longer clobbers a racing commit — it flushes through
   `save_graph_guarded` (`src/commands_graph_ops.rs`) — but it still does not
   route, and neither does `ingest`: over `call_tool` they would land at agent
   trust, and kern carries no caller identity by decision (2026-07-22), so the
   route-or-stay-local choice is owed. `intake drain` got its tool
   2026-07-21 and routes. Open as `ROADMAP.md` item 9 on `ingest`/`link` alone.
6. **GNN training has no GPU path** — linear in edges and off the tick since
   item 28 (11.6s at 4096, was 79.7s); `gnn_train_refused` now reaches `kern health`.
7. **Distill prompt** is one-shot and global — per-kind prompts +
   chunking for long deltas would raise claim quality.
8. (retired 2026-07-21 — one scrub pass per sweep, not one per victim)
   `HnswIndex::delete` (`src/hnsw.rs:136`) drops the node and pushes the
   slot to `pending_scrub`; `scrub_pending` (`:153`) clears every dead slot in a
   single walk, and only then may a slot enter `free`, so nothing can alias it.
9. (retired 2026-07-21 — the LLM rerank left with the answer leg) a small
   cross-encoder trained on `degrade` feedback could replace it.
10. **Only `GnnPropagate` reports a contained failure** — the panic guard covers
    every task, but a task that returns early instead of dying is still
    invisible outside its own logs.

---

*Scraped from source at `v1.3.0`, last reconciled against the tree 2026-07-27.
Update this file when a subsystem's public surface changes — it is the canonical
feature inventory. The stamp is a date, not a commit: a commit hash here ages
into a lie the moment the next one lands, and nothing checks it.*
