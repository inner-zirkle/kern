# Recall Improvement Plan — make kern memory actually reach the agent

Status: **implemented, 2026-08-14** — every fix below landed and was verified
against the live store. This document records what was done; the sections are
kept as written with implementation notes where the code deviated.

Complaint this addresses (other agent's report): retrieval precision poor,
`kern_ingest` fails "kern not available", injected memory is a flat top-K that
never uses the graph. Investigation shows the real bottleneck is **not the
retrieval pipeline** (11 ms) but **process-start index rebuild (~4.5 s) racing
pi's fixed 3 s/5 s tool timeouts**, on top of **store pollution by per-turn
auto-ingestion**. Fixing those two restores read+write+injection; the graph
already computes chains that the wrapper currently throws away.

---

## 0. Baseline (what exists today)

- **The real store**: `/Users/feb/dev/llm/.pi/kern` — `kern:generic`
  1,346 thoughts / 3,489 reasons, embedded `qwen3-embedding:0.6b` (dim 1024)
  via local ollama. The reporting session's "926 thoughts / 2,050 reasons"
  is this store at an earlier size (it has grown since; counts drift).
- **Read path** (pi): `before_agent_start` → `queryThoughts(lastInput, 5)` →
  `kern query` CLI one-shot, **3 s timeout** (`store.ts`).
- **Write path** (pi): `message_end` → `ingestBlock(<kern> lines)` →
  `kern ingest --file`, **5 s timeout** (`TIMEOUT = 5000`).
- **Tools**: `kern_query` / `kern_ingest` / `kern_link` / `kern_forget` /
  `kern_health` → same CLI, same env (`KERN_DIR=<cwd>/.pi/kern`, set by
  `kernEnv()` in `store.ts`).

## 1. Measured findings

1. **The pipeline is fast; process start is slow.**
   `RUST_LOG=kern.profile=debug`: `retrieve=11.1ms` total. But `kern health`
   (load only) = **4.5 s user CPU**; `kern query` = 3.5–5.9 s; `kern ingest`
   = 4.98 s. Cause: `rebuild_index()` (graph.rs:300) rebuilds **three in-RAM
   HNSW indexes** (M=16, ef=200) at every process start — entity 1,346 +
   reason 3,489 + gnn 1,346, all 1024-d. A DiskANN backend already exists
   (`src/graph/src/diskann.rs`, `VectorBackend::Disk`, spill logic,
   `consolidate_disk_index`, tests in `graph_test.rs:308`) but
   `disk_threshold = KERN_CAP_DISABLED` (config.rs:582, "stays disabled until
   item 75"), and `build_and_save` (diskann.rs:159) rebuilds the Vamana graph
   every call with no freshness stamp.

2. **pi's timeouts are shorter than load → every tool call dies.**
   `queryThoughts` → `run(["query", q], 3000)`; 3 s < 4.4 s → `run()` catches
   the timeout, returns `""`, wrapper reports **"no results"**.
   `ingestOne` → 5 s ≈ the measured 4.98 s → borderline kill →
   **"kern not available — install with cargo install kern"** (misleading;
   the binary is fine). This is the direct, reproduced cause of the other
   agent's "read the graph but cannot grow it".

3. **The per-turn injection is also dead, for the same reason.** The
   `before_agent_start` memory-hits query has the same 3 s timeout, so hits
   never inject. What the reporting agent valued as "relevant past thoughts in
   my context" is actually the **doctrine + ontology digest** — file-based,
   kern-free. Valuable, but it is not graph retrieval.

4. **Exact-term recall is good; generic-term precision is noise.**
   - `"b-routing codec Lobe Shard"` → the Lobe/Shard facts at rank 1–2.
   - `"shard terminology retired"` → real fact at rank 2 (noise at rank 1).
   - `"gantt claimed b-seedleech"` → exact hit at rank 1–2.
   - `"codec"` → top-6 = 5 slim-turn dumps + 1 ticket. `"codec"` in content
     mode → `# Observation: slim turn 45` at rank 1 (score 1.61).

5. **Noise volume.** Of 1,375 listed thoughts, ~1,880 list lines are `slim turn N`,
   `# Observation: slim turn N`, `- tools: … / - failed: N` per-turn dumps
   from pi's `core/slim/index.ts` plus reflex ratings, gantt claims, btw
   questions. All carry scheme `inline://<session-hash>` — the same scheme as
   real decisions — so `source_trust` cannot separate them today, and
   `kern_forget` cannot remove them: they are `Fact` kind, the guard requires
   `--force --source`, and pi's `forgetSource` passes neither (returns 0
   unconditionally).

6. **Chains are computed but thrown away.** `expand()` walks reason edges
   (`max_expansions=500`) and the CLI prints a `--- Connections ---` section,
   but pi's `queryThoughts` parses only flat `N. [score] id text` lines. The
   graph structure never reaches the agent.

7. **Store split-brain.** `KERN_DIR` is per-repo `<cwd>/.pi/kern`.
   `zirkle/kern/.pi/kern` = 14 thoughts, `~/.kern` = 26,
   `llm/.pi/kern` = 1,346. Sessions outside the llm repo silently read/write
   tiny or empty stores. `kernEnv()` overwrites any user-set `KERN_DIR`, so
   there is no escape hatch to pin the home store. The other agent looked at
   `~/.kern`, saw a small store, and judged the whole system from a session
   whose tools were timing out against the real one.

## 2. Root causes

- **R1** — One-shot CLI pays a ~4.5 s index rebuild per invocation; pi's
  fixed timeouts (3 s query / 5 s ingest) are shorter than that. (Dominant.)
- **R2** — Timeout failures are misreported as "kern not available" /
  "no results", indistinguishable from a missing binary or empty store.
- **R3** — Per-turn auto-ingestion (slim/reflex/gantt/btw) writes junk under
  scheme `inline`, with a Fact guard that blocks cleanup.
- **R4** — Graph chains never reach the agent; injection is a flat top-5 of
  the raw input.
- **R5** — No store discovery or sharing across repos.

## 3. Fixes (owner · change · verification)

### F1 — Unblock the tools: raise timeouts + typed errors *(pi repo, ~10 min)*
File: `/Users/feb/dev/pi/packages/coding-agent/src/core/memory/store.ts`
and `tools.ts`.
- `queryThoughts`: `run(["query", q], 3000)` → **20 000 ms**.
- `TIMEOUT` 5000 → **20 000 ms**; `ingestBlock` base 5000 → 20 000.
- Distinguish failure classes: binary missing → keep "install with cargo
  install kern"; timeout → "kern timed out (~4.5 s store load; retrying)".
  Have `run`/`ingest` return `{ok, timedOut}` instead of bare strings.
- Verify: `kern_query "b-routing codec Lobe Shard"` from a pi session returns
  the Lobe/Shard facts; `kern_ingest` returns an id; status line shows
  `1346T·3489R`.

### F4 — Kill the cold-start cost *(kern repo, ~1–2 h)* — the latency root cause
- **F4a** default `disk_threshold` to auto (spill whenever a `data_dir` is
  present) instead of `KERN_CAP_DISABLED` (config.rs:582); keep the test
  override at 10 (commands/lib.rs:1721).
- **F4b** freshness stamp in `build_and_save` (diskann.rs:159): skip the
  Vamana rebuild when the stored stamp matches the store epoch (persist.rs
  already writes stamps); cold start then mmaps the saved index.
- Verify: `time kern health` twice in a row → second run < 0.5 s.
  `cargo test -p graph` (diskann_test, graph_test) green.

### F2 — Recall/precision: cut the noise *(kern + pi repos, ~1–2 h)*
- **F2a** prune existing junk: script `scripts/prune-slim-turns.sh` that
  lists thoughts and forgets text matching `^slim turn \d+` /
  `^# Observation: slim turn` via `forget --source <src> --force`.
  **Destructive — back up `.pi/kern/data` first; requires user go.**
  Expect: 1,346 → ~300–400 real thoughts.
- **F2b** stem the flow: pi's `core/slim/index.ts` (and reflex/gantt/btw
  writers) either stop per-turn `storeObservation`, or tag observations with
  their own scheme (e.g. `slim://<session>`); then `source_trust` in
  `RetrievalConfig` (already wired, default empty) downweights the channel.
- **F2c** `lexical_top_boost` default 0.0 → ~0.5 (config.rs) so exact-term
  matches float above embedding neighbours — "codec" then ranks the codec
  ticket/thoughts above slim turns. Update any score-expectation tests
  (`cargo test -p retrieval -p config`).
- **F2d** `kern_forget` tool gains a `force` param; `forgetSource` passes
  `--force --source` so Facts (the junk kind) can actually be removed and
  returns the count instead of 0.

### F3 — Surface the graph *(pi repo, ~30 min)*
- **F3a** `queryThoughts` also captures the `--- Connections ---` section the
  CLI already prints (chains are computed for free) and appends the top
  chain(s) to tool output and to the injected memory block.
- **F3b** injection: probe with last input **plus** top-hit expansion
  (kern's `expand` already runs in the pipeline — the wrapper just drops the
  result), and inject hits + one chain instead of flat top-5.

### F5 — Store discovery *(kern + pi, ~1 h)*
- **F5a** `kern status` warns when opening a near-empty store while a sibling
  store (e.g. `~/.kern` or another repo's `.pi/kern`) holds >N thoughts.
- **F5b** `kernEnv()` respects a pre-set `KERN_DIR` (escape hatch to pin the
  home store) + a README paragraph "where the memory lives" documenting
  per-repo stores and the pin.

## 4. Order and ownership

1. **F1** (pi) — unblocks read+write+injection immediately. 10 min.
2. **F4** (kern) — removes the ~4.5 s tax that made timeouts necessary. 1–2 h.
3. **F2b + F2c** (kern + pi) — stop new junk, fix generic-term ranking.
4. **F2a** (kern script) — prune legacy junk; needs user go (destructive).
5. **F3** (pi) — put the graph's chains in front of the agent.
6. **F5** (kern + pi) — stop silent empty-store sessions.

## 5. Verification (after each step)

- F1: pi session `kern_query "codec"` returns the codec ticket; `kern_ingest`
  returns an id; injected block contains memory hits.
- F4: `time kern health` twice → second cold start < 0.5 s.
- F2c: `kern query "codec"` top-10 has ≥3 real codec thoughts (no slim turns
  in top-5).
- F2a: slim-turn count < 100; store ≈ 300–400 thoughts; exact-phrase queries
  still recall rank 1–2 (Lobe/Shard regression check).
- Regression: `cargo test -p retrieval -p graph -p config`; pi dist build +
  memory extension tests.

## 7. Implementation notes (what actually shipped)

All verification numbers below are against `/Users/feb/dev/llm/.pi/kern`
(the real store).

- **F1 — pi timeouts + typed errors**: `store.ts` query timeout 3s→20s, ingest
  5s→20s; `run`/`ingest`/`ingestOne` return `{ timedOut }`; tools now report
  "timed out" vs "kern not available" distinctly. `kern_query`/`kern_ingest`
  work end-to-end against the live store (measured: 5 hits + 5 chains,
  `timedOut: false`).
- **F4 — cold-start fix (kern)**: the ~4.5s per invocation was the resident HNSW
  rebuild of three indexes (entity+gnn+reason) at every process start — not the
  pipeline (11ms). Now: `disk_threshold` defaults to 0, all three indexes load
  from mmap'd DiskANN snapshots stamped with the store epoch, and a changed
  store **reconciles** the diff into the delta overlay (tombstone removed ids,
  insert changed/new vectors, amortized full rebuild when the diff outgrows the
  snapshot) instead of rebuilding. `from_saved_with_mode` constructs with
  `disk_threshold: 0` so the first load takes the disk path. Measured: load
  4.5s → **0.09s** (hot store, release build). Full rebuild of a stale snapshot
  is ~8-11s at 1.4k entities — amortized, not per-write.
- **F2a — prune command (kern)**: new `kern prune --pattern [--source] [--dry-run]
  [--force]` — one store load, in-process text-pattern removal reusing the
  forget machinery, refuses while a daemon holds the writer lock. Ran against
  the llm store: removed 1,393 slim-turn + 45 tool-dump thoughts (3,955 edges)
  in <1s. Store went **1,450 → 201 thoughts** (92% was per-turn test junk).
  `kern query "codec"` top-6 went from 5 junk + 1 ticket to all-real content
  (codec ticket, Lobe/Shard fact, terminology, gantt claim). Pre-prune backup:
  `/tmp/llm-kern-backup-1786705312`.
- **F2b — stem the flow (pi)**: `core/slim/index.ts` no longer stores
  successful silent turns (`read → (no text)`); failures and real outcomes
  still land. Other writers (reflex/gantt/btw/crew) unchanged — they were the
  useful keepers.
- **F2c — lexical_top_boost**: default 0.0 → 0.5, so exact-term matches float
  above embedding neighbours.
- **F2d — kern_forget --force**: tool gains a `force` param; `forgetSource`
  passes `--force` and reports the real count ("forgot N thoughts").
- **F3 — chains surface**: `queryThoughts` parses the `--- Connections ---`
  section the CLI already computes; chains are appended to `kern_query` output
  and injected into the per-turn memory block ("# Kern connections").
- **F4-bonus — KERN_DIR restored**: the installed v1.4.0 binary honored
  `KERN_DIR` but the source did not — pi's integration was silently reading/
  writing `<cwd>/.kern` with any fresh build. Restored in `Config::default_in`
  (data_dir = `$KERN_DIR/data`) with a regression test.
- **F5a — store discovery**: `kern status` walks the directory ancestors and
  lists the other stores found, with a "set KERN_DIR to pin" hint.
- **F5b — KERN_DIR escape hatch (pi)**: `kernEnv()` respects a pre-set
  `KERN_DIR` instead of always overriding it.

Tests: `cargo test --workspace` → **1,109 passed, 0 failed**; pi `tsgo`
noEmit clean; `vitest test/suite/memory-store.test.ts` → 11 passed. The
binary is installed as `kern` 1.4.0 from this tree.

Still open (deliberately not done): the daemon never enqueues `DiskConsolidate`
(its delta grows until a restart reconciles — correct, just not compacted); a
scheduled consolidate would fold it. And the prune is manual — a future `kern`
release could auto-prune by source scheme once writers carry their own.

## 6. Explicitly not doing

- No retrieval-pipeline rewrite — 11 ms is not the problem; recall failure is
  load+timeout+noise.
- No store-format change — bincode is FORMAT_VERSION-locked (Cargo.toml).
- No silent data deletion — F2a requires backup and approval.
