# kern

**A self-learning memory daemon for AI agents.** One long-running process per
working directory owns a knowledge graph that your agent writes durable facts
into, keeps itself small without gardening, and serves back on recall.

kern is not a vector store you bolt onto an app. It is a *memory substrate*: the
writes are caller-driven — kern captures nothing on its own — and everything
after them it does on its own: compaction, decay, GC, clustering, re-ranking
from what you actually use, and (optionally) federation across machines.

```
MCP ingest ─────────────────────► typed claims ─┐
.kern/intake/ drop → distill (LLM) → typed claims ┴→ graph → recall
```

---

## What it does

- **Two ways in, both caller-driven.** An agent calls the `ingest` MCP tool to
  store a durable fact directly — the primary path. Or drop a conversation delta
  (a `.txt` file) into `<cwd>/.kern/intake/` — the daemon drains it and runs one
  LLM distillation pass that pulls out durable *facts*, *decisions*, and
  *preferences* as typed claims and ingests each into the graph. The drop dir is
  agent-agnostic: your agent, a wrapper, or a script writes it — kern ships no
  writer of its own, and captures no session automatically. Nothing is lost on an
  LLM outage — a queued delta stays until it succeeds.

- **Recalls into context.** Recall is the `query` MCP tool: relevance-targeted
  against the live graph, with provenance on every result.

- **Compacts itself.** Every access deposits a **heat** trace, and nothing else
  does — the tick's pulse schedules maintenance without depositing, so retention
  never tracks tree position; heat decays lazily with age, not per tick. A stigmergy GC evicts
  cold, stale, non-durable thoughts (Facts are immune) and spills them to a
  capped cold tier before dropping them — a latest-wins keyed table holding the
  newest 50k entries, so recent evictions stay recoverable while the very oldest
  eventually age out. Rows pushed out past that cap are counted and reported by
  `health` (`src/base/store.rs:718`), so the tail's loss rate is observable
  rather than silent. Spill-before-drop needs a store: with no store bound
  (in-memory mode) the victim is dropped outright (`src/tick/stigmergy.rs`) —
  dropping *is* the intended memory bound there, and it is counted too, so an
  in-memory deployment cannot quietly read as a durable one (`unspilled_drops`
  on `health`). Similar thoughts cluster into
  child kerns. The hot graph stays small; the long tail stays cheap.

- **Remembers across time.** Knowledge carries a bi-temporal window. When a new
  claim updates or contradicts a stored one of the same kind, kern supersedes the
  old rather than deleting it — the invalidated revision stays as history, stamped
  with when it stopped being true. A `query` can ask `as_of` a past instant to
  recover what was believed then, or `include_history` to follow the supersede
  chain back through prior revisions. The classification runs in the background
  tick, so recall stays LLM-free. `as_of` is exact over both tiers: a cold row
  carries `valid_from`/`valid_to`/`invalidated_at` beside the entity
  (`src/base/store.rs:148-153`), so an evicted revision answers the same window it
  answered while hot.

- **Federates (opt-in).** Multiple nodes share knowledge over LAN gossip with no
  coordinator. Each node heartbeats peers and merges entity bodies via a
  content-addressed CRDT — a thought ingested on node A becomes searchable on
  node B under the same content-hash id. Manually seeding `peers` is the
  reliable path today; multicast discovery only pairs nodes that share the same
  `network_id`. Off by default.

- **One graph per directory.** The daemon is per-cwd. Each project gets its own
  isolated memory; no cross-project contamination, multiple daemons per host.

---

## How it works

### The graph

kern stores two things:

- **Thoughts** — factual chunks and LLM-extracted claims. Typed (`normal`,
  `fact`, `document`) and weighted by confidence + heat.
- **Reasons** — justified edges between thoughts. The *why* connecting two
  facts, not just a similarity score.

Ids are **content hashes**, so identical content is the same node everywhere —
existence is a set union, which is what makes conflict-free merge across nodes
work.

### Retrieval

A query runs a hybrid pipeline, all hand-rolled, dependencies deliberately
minimal:

1. **Seed** — vector (HNSW) + lexical (BM25) candidate generation. For a node
   present in both indices the dense score blends the content vector with a
   **GNN** vector 0.4/0.6 (`src/base/search.rs:60`); a node in only one index
   keeps that index's score. The GNN vector is what a background tick keeps
   re-embedding from graph structure.
2. **Expand** — walk reason edges out from the seeds
   (`src/retrieval/expand.rs:178`) and return the traversal chain as provenance.
   Measured: adding
   a reason edge between two thoughts changes no delivered ranking — linked and
   unlinked pairs score identically to four decimals. The edge is created and is
   walkable; it does not reach the score. Open — see `docs/windmill/ROADMAP.md`.
3. **Fuse** — reciprocal-rank fusion of the vector and lexical lists, with
   PageRank centrality weighting the fused seeds.
4. **Diversify** — drop near-duplicates so the `k` results actually differ.
5. **Deliver** — passages, enriched edges, and path chains. No synthesis:
   the whole read path is LLM-free, and the calling agent synthesizes.

Cold-store results fill remaining slots (marked `cold:true`) when the hot graph
returns fewer than `k`.

### The daemon

`kern --daemon` exposes its surface two ways:

- **MCP** (stdio + HTTP/SSE) for external clients.
- a typed **`KernRpc`** (generated by this repo's own `service!` macro in `src/trnsprt/`) over a per-cwd socket for other local clients.

A background **tick** (default 60s) drives decay, eviction, and clustering — an
idle daemon still maintains itself. A task that panics is caught, counted and
named rather than taking the loop down with it (`src/tick.rs:65`), so one bad
maintenance pass costs one task instead of every future tick; `health` reports
the panic and failure counts with the last of each. Persistence is **LMDB** (via
[heed](https://github.com/meilisearch/heed)) — an ACID, multi-process embedded
KV. Hot graph and cold tier live together in one LMDB environment
(`data.mdb` + `lock.mdb`) per data dir; vectors are stored int8, values are
`zstd(bincode)`. LMDB is single-writer: readers never block, writers serialize,
and a guarded-flush protocol keeps a stale in-memory snapshot from overwriting
newer on-disk state. HNSW, the GNN, beam search, gossip, and the MCP server
are all written from scratch.

### The hub

One machine-level supervisor owns node lifecycle. `kern mcp` asks the hub for
the project's daemon — auto-starting the hub if none runs (`[hub] auto_start =
false` opts out) — and the hub spawns the node, adopts an externally started
one, or hands back the live socket. `kern hub status` lists
tracked nodes; `kern hub unload [root]` shuts one down gracefully
(save-then-exit over RPC). Nodes idle past `--idle-unload-secs` (default 30
min) are unloaded automatically and respawn on the next connect, so memory
tracks the active set, not the installed set. `kern hub merge <src> <dst>`
folds one project's graph into another (offline CRDT union; src untouched);
`kern hub stop` ends the hub, leaving nodes up. The data path stays direct
client→daemon — the hub is connect-time only. If the hub is disabled or
unreachable, everything falls back to the pre-hub behavior.

---

## Using it

### Quickstart

**Prerequisites:** a local [Ollama](https://ollama.com) with the default
models pulled:

```bash
ollama pull qwen3-embedding:0.6b  # embeddings (default)
ollama pull granite4:3b       # distillation / reasoning (default; write path only)
```

> **WSL2 with Ollama on the Windows host:** the default
> `http://localhost:11434` resolves inside the WSL VM, where nothing listens.
> The Windows host is the VM's default gateway — `ip route show default`
> prints it (e.g. `172.27.176.1`). Point `[embed]`/`[reason]` `url` in
> `kern.toml` (and the eval runners' `--embed-url`) at
> `http://<gateway-ip>:11434`, and make sure Ollama listens beyond loopback
> (`OLLAMA_HOST=0.0.0.0` on the Windows side).

**1. Install the binary.** A prebuilt binary for your platform (built by CI and
published to GitHub Releases):

```bash
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/yesitsfebreeze/kern/master/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/yesitsfebreeze/kern/master/install.ps1 | iex
```

> Or build from source (needs a Rust toolchain): `cargo build --release` →
> `target/release/kern`, or `cargo install --path .`.

**2. Register the MCP server with your client.** `kern mcp` attaches to a
running daemon if one exists, and otherwise auto-spawns a detached daemon for
the current directory — so this one command is all you need to bring kern up
(the installer prints the exact path). A detached child writes its stdout and
stderr to an owner-only append log under `<data_dir>/logs/`
(`src/config/detached_log.rs:23`) — `hub.log` for the machine hub, `daemon.log`
for a node, `.kern/data/logs/` by default — so a spawn that dies leaves a trace.
`kern status` reports which of the two happened, and whether anything holds the
writer lock; the offline admin commands refuse while it is held.
One caveat is still real: if the attach fails, `kern mcp` falls back to serving
the store itself (`src/commands/mcp_cmd.rs:47`), a second writer against the
same LMDB environment. A stale flush is refused rather than clobbering newer
state, but the two writers still exist. Add it as a stdio MCP server in your
client's config:

```json
{
  "mcpServers": {
    "kern": { "command": "kern", "args": ["mcp"] }
  }
}
```

**3. Wire the intake (optional).** kern is agent-agnostic: any tool that
writes a conversation delta to `<cwd>/.kern/intake/*` feeds the intake. Wire it
whatever way your client supports (a hook, a wrapper, or a manual `kern ingest`).
Recall needs no wiring — it is the `query` MCP tool.

**4. Know where the graph lands.** No config file is needed — every default
(embedding, reasoning, intake, tick) works out of the box against a local
Ollama. The daemon pins itself to the nearest ancestor holding `.git`, else the
nearest holding `.kern/`, else the launch directory
(`src/config/mod.rs:172-183`), and creates `.kern/data/` there on open
(`src/base/store.rs:314-315`) plus `.kern/intake/` for the drop dir. `mkdir .kern`
in a directory that has neither marker if you want the graph somewhere the walk
would not have chosen. (A `<cwd>/.kern/kern.toml` is only for overriding
defaults — see *Configure* below.)

**5. Seed the graph** (see *Seed the graph* below), then start a session. From
then on, store facts by calling the `ingest` MCP tool (or drop transcripts into
`.kern/intake/`), and pull them back with `query`. kern captures nothing on its
own — the writes are yours to make.

To verify it's working, call the `health` MCP tool from your session. Prefer the MCP tools
over the `kern <subcommand>` CLI for live state — the CLI reads the on-disk
graph directly and can race the running daemon.

kern is alpha: store and wire formats change without migration paths. A store
written by an older build is rejected at load — wipe `.kern/data` and reingest.

### Configure

Configuration is **optional** — with no config file at all, kern ingests and
recalls with the defaults shown below. To override, create
`<cwd>/.kern/kern.toml` (project scope) or `<XDG_CONFIG>/kern/kern.toml`
(user scope).

The two scopes **deep merge per key** (`src/config/io.rs:46`): a project that
sets one field of a section keeps the user's other fields in that section.
Arrays and scalars are leaves — the project value replaces, never appends, so
`gossip.peers` and `watcher.roots` are complete lists rather than accumulators.
One deliberate exception: a scope that sets a section's `url` does not inherit
that section's `key` (`src/config/secrets.rs:15`). A cloned repo that redirects
an endpoint must supply its own credential or go without.

An **absent** config is legitimate and defaults silently. A config that is
present but unreadable or invalid **aborts startup** with exit 78 (`EX_CONFIG`)
and the offending key on stderr (`src/main.rs:16`) — booting on settings known
to be wrong is failing silently, not failing open. `--help` and `--version`
still answer in a repo whose config is broken.

```toml
[reason]
# LLM for distillation. Local Ollama.
url = "http://localhost:11434"
model = "granite4:3b"       # default (small, fast, reliable)

[embed]
# Embedding model. Local Ollama.
url = "http://localhost:11434"
model = "qwen3-embedding:0.6b"  # default; dimension locks the graph (use `kern reembed` to switch)

[answer]
# User-facing /ask oracle (streamed answer over MCP). Latency-critical, only
# glues retrieved nodes into prose → smallest model that grounds. Uses Ollama's
# native /api/chat (capped context, kept GPU-resident) only when the url is local
# and does not end in /v1 — otherwise it speaks OpenAI-compatible /v1/chat/completions,
# so a remote or vLLM endpoint works here too. url/key blank → fall back to
# [reason]'s endpoint, so a single local Ollama needs no extra wiring.
model = "granite4:3b"       # default (same as [reason])

[intake]
enabled = true          # self-learning (ON by default; set false to opt out)

[tick]
interval_secs = 60      # self-compaction cadence (0 = event-driven only)

[gossip]
enabled = false         # self-distribution (opt-in)
addr = "0.0.0.0:7400"
discovery = true
discovery_port = 7475
# network_id = "team-alpha"  # optional shared discovery pool id (omit for an isolated per-daemon id)
peers = []
```

The store stamps the embedding model and vector dimension that produced its
vectors and re-checks them on open and on every flush
(`src/base/store.rs:417`). A swap is reported, never silently tolerated: the
`health` tool carries `embed_model`, `embed_dim` and `embed_mismatch`, and the
CLI prints `MISMATCH`. A query vector whose dimension disagrees with the
index returns no hits rather than nonsense, counted with a throttled log line
(`src/base/search.rs:23`). An unstamped store adopts the configured model — that
is not a mismatch. `kern reembed` rewrites the vectors and stamps the model it
actually embedded with, not the configured one (`src/commands/reembed.rs:59`).

> **Before enabling gossip**, read the
> [Security](https://yesitsfebreeze.github.io/kern/concepts/security) page —
> the full trust model, including exactly what a malicious peer can and cannot
> do. Federation is unauthenticated and unencrypted today: enable it only on a
> network segment where you trust every host.

### Intake & recall

kern is agent-agnostic. There is no client-specific plugin; both halves are
things any client can already do — write a file, call an MCP tool.

- **Intake** — drop a conversation delta as a `.txt` file in
  `<cwd>/.kern/intake/`. The daemon drains it, distills typed claims out of it,
  and ingests them. Write the file however your client supports (a hook, a
  wrapper, or a manual `kern ingest`); kern only cares about the file.
- **Recall** — call the `query` MCP tool. It is relevance-targeted against the
  live graph and keeps provenance on every result.

Neither is gated on a pre-existing `.kern/`. A daemon pins itself to the nearest
ancestor holding `.git`, else the nearest holding `.kern/`, else the launch
directory (`src/config/mod.rs:172-183`), and then creates the store and intake dirs
it needs. So a single global registration does reach every project you open
`kern mcp` in, and every one of them gets a `.kern/` — that is the cost of the
registration being global, not something kern avoids.

**Requirements:** the `kern` CLI on `PATH` and a running embedding endpoint
(Ollama by default) for recall.

### Seed the graph

Once, via the MCP tools against the running daemon (not the CLI, which races the
daemon). From an MCP session in the project:

1. Add a few gravitons — call `graviton` (action `add`) with a `name` and a
   `text` for each focus area the graph should gravitate around, e.g.
   *"decisions"*, *"project state"*, *"preferences"*. The text can be a one-line
   description or a full document/message — it is embedded whole as the
   graviton's pull vector. An optional `mass` (default `1.0`) makes a graviton
   pull harder: ingest routes by `distance / mass`, and query ranking boosts
   thoughts near a graviton by `gravity_weight * mass * cos`. Memories that
   match no graviton land in `generic`; dense `generic` clusters auto-promote to
   new gravitons over time.
2. Optionally register extra claim kinds beyond the built-ins (`preference`,
   `decision`, `project`, `fact`, `code-fact`, `reference`, `procedural`) —
   call `claim_kind` (action `add`) once per custom kind; distillation offers
   registered kinds to the LLM alongside the built-ins.

After seeding, populate the graph by calling the `ingest` MCP tool during a
session, or by dropping transcripts into `.kern/intake/`. kern ships no session
hook — the write is always a caller's call.

### MCP tools

| Tool | Purpose |
| ------ | --------- |
| `query` | Search the graph. Scored thoughts + optional LLM answer. Filter by `mode`, `kind`, `source`, time range, `min_conf`, and `as_of` (bi-temporal point-in-time); set `include_history` to also return superseded revisions (flagged `history:true`). |
| `ingest` | Add text. Supports `object_id` update semantics and a free-text `hint` for chunking context. |
| `link` | Create a reason edge between two thoughts (LLM writes the reason if blank). The edge is stored and walkable and shows up in a result's chain; it does not change ranking today. |
| `forget` | Remove a thought and cascade its edges. Facts are immune. |
| `degrade` | Name the thought at the end of a bad result and every edge incident on it decays, hardest first; an edge that falls below threshold is removed (`degrade_entity_reasons`, `src/commands/graph_ops.rs:468`). Entity-scoped — there is no way to name one path among several. |
| `move` | Relocate a thought to another kern by `id` and `to_kern`, carrying its outgoing edges and restamping cross-kern references. |
| `graviton` | Manage gravitons (named focus attractors; replaced the single per-kern "purpose"): `list` (default), `add` (name + text — phrase or full document — + optional mass), `remove` (name). |
| `claim_kind` | Register/remove a claim kind; registered kinds extend the built-in set distillation may emit. |
| `health` | Graph stats plus degradation signals: thought/edge/unnamed counts, tick queue depth and latency, task panics and failures with the last of each, cold evictions, and the stored embedding model/dimension with its `embed_mismatch` flag. |
| `pulse` | Trigger a clustering pass across the kern tree. |
| `gc` | Reap empty/orphan kerns from the running daemon's graph and persist, live. Returns before/after kern counts and the `data.mdb` size. |
| `intake_drain` | Drain `.kern/intake/` now instead of at the next poll: distil queued `.txt` transcripts, ingest other readable files whole, archive what committed. `kern intake drain` routes through this so only one process ever drains the queue. |
| `setup` | Returns wiring instructions for the calling agent — seed gravitons, install the capture rule, verify — with the steps already done marked. kern never writes a host's config itself (`src/mcp/tools_setup.rs:3`). |

---

## kern vs. traditional RAG

Traditional RAG is a pipeline you operate: chunk documents, embed them, stuff a
vector DB, and on every query do top-k cosine + prompt-stuff. kern is a memory
that operates itself.

| | Traditional RAG | kern |
| --- | --- | --- |
| **Ingestion** | Manual: you run a chunk-and-embed job over a corpus. | Caller-driven: an agent calls `ingest`, or a dropped transcript distills into typed claims via the intake — no re-indexing job. |
| **Unit stored** | Raw text chunks. | Distilled facts/decisions/preferences + *reason edges* between them. |
| **Retrieval** | top-k vector similarity. | Hybrid vector + BM25 with GNN-blended seeds, edge expansion, RRF + PageRank fusion, optional LLM rerank, diversify. |
| **Structure** | A flat bag of vectors. | A knowledge graph — a result carries the reason chain connecting it back to a seed, so recall shows *why* one fact reaches another. The chain is provenance, not score: an edge changes no ranking today (see *Retrieval* above). |
| **Growth** | Index grows unbounded; you re-index and prune by hand. | Self-compacting: heat decay + stigmergy GC + clustering keep the hot graph small; a capped cold tier preserves the recent tail. |
| **Staleness** | Stale chunks linger until you rebuild. | Cold, non-durable thoughts decay and evict on their own; Facts persist. |
| **Feedback** | None — a bad chunk keeps ranking. | `degrade` punishes a bad result's delivered score (entity-scoped, not path-scoped); access heat re-ranks what you actually use. |
| **Conflicts / sync** | Single store; multi-node needs external infra. | Content-addressed CRDT + gossip, no coordinator — but no anti-entropy: each heartbeat ships only the hottest 32 entities, so cold ones may never propagate and a node that rejoins after a partition never catches up. |
| **Scope** | One global index. | One graph per working directory. |

The short version: RAG gives you **search over a corpus you maintain**. kern
gives you **memory that maintains itself** — it decides what is durable, forgets
what isn't, and stores the reason connecting one fact to another so a result can
show its chain instead of arriving as a bare nearest neighbor.

---

## Status

Intake, recall and self-compaction run today. Self-distribution is wired and
enableable, but narrower than the design: entity-body sharing is verified on a
single host with manually seeded `peers` (the reliable path), and multicast
auto-discovery only pairs nodes sharing the same `network_id`. The Delta,
Question and Pulse senders and the fetch RPC are all live; what is missing is
**anti-entropy** — the sync
leg ships only the hottest 32 entities per heartbeat, so a cold entity may never
propagate and a partitioned node that rejoins never catches up. Federation
tuning at scale (batch size, push vs. pull) is open alongside it; see
`docs/windmill/ROADMAP.md` — "Anti-entropy". Version `1.1.0`.

**Measurement.** `tests/e2e/test_recall.py` scores recall@1 / recall@5 / MRR over a
corpus the test itself authors, with no LLM anywhere in the scoring loop — it
ingests the facts, so it knows the right answer for each probe, and scoring is
rank arithmetic over the binary's own stdout. Currently 0.9306 / 0.9722 /
0.9471, reproducible bit-for-bit; CI gates on floors below those. Two limits
travel with that number and cannot be dropped from it. The floors make it a
**regression detector, not a quality claim** — it can say kern got worse, never
that kern is good, and it is comparable to nothing anyone else publishes.
And the embedder in the loop is `tests/e2e/fake_llm.py`'s feature-hashed bag of words,
deterministic and semantically empty by design, so the number measures kern's
retrieval machinery over a fixed lexical signal, not a real embedding model's
semantics. `tests/e2e/test_invariants.py` asserts one property per `docs/windmill/VISION.md`
criterion, and the properties kern does not yet satisfy are recorded there as
skips and xfails rather than dropped.
