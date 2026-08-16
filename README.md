# kern

**A self-learning memory daemon for AI agents.** One long-running process per
working directory owns a knowledge graph that your agent writes durable facts
into, keeps itself small without gardening, and serves back on recall.

kern is not a vector store you bolt onto an app. It is a *memory substrate*: the
writes are caller-driven — kern captures nothing on its own — and everything
after them it does on its own: compaction, decay, GC, clustering, re-ranking
from what you actually use.

```
kern ingest (CLI → daemon) ─────► typed claims ─┐
.kern/intake/ drop → distill (LLM) → typed claims ┴→ graph → recall
```

---

## What it does

- **Two ways in, both caller-driven.** An agent runs `kern ingest` to store a
  durable fact directly — the primary path; the CLI is the agent surface, and
  each verb is a thin dispatch to the project's daemon. Or drop a conversation delta
  (a `.txt` file) into `<cwd>/.kern/intake/` — the daemon drains it and runs one
  LLM distillation pass that pulls out durable *facts*, *decisions*, and
  *preferences* as typed claims and ingests each into the graph. The drop dir is
  agent-agnostic: your agent, a wrapper, or a script writes it — kern ships no
  writer of its own, and captures no session automatically. Nothing is lost on an
  LLM outage — a queued delta stays until it succeeds.

- **Recalls into context.** Recall is `kern query`: relevance-targeted against
  the live graph, with provenance on every result. `kern query --all` fans one
  query out across every kern the machine hub knows.

- **Compacts itself.** Every access deposits a **heat** trace, and nothing else
  does — the tick's pulse schedules maintenance without depositing, so retention
  never tracks tree position; heat decays lazily with age, not per tick. A stigmergy GC evicts
  cold, stale, non-durable thoughts (Facts are immune) and spills them to a
  capped cold tier before dropping them — a latest-wins keyed table holding the
  newest 50k entries, so recent evictions stay recoverable while the very oldest
  eventually age out. Rows pushed out past that cap are counted and reported by
  `health` (`src/store_core/src/lib.rs:775`), so the tail's loss rate is observable
  rather than silent. Spill-before-drop needs a store: with no store bound
  (in-memory mode) the victim is dropped outright (`src/tick/src/tick_stigmergy.rs`) —
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
  (`src/store_core/src/lib.rs:145-147`), so an evicted revision answers the same window it
  answered while hot.

- **Local memory, no network.** Nothing kern stores ever leaves the machine:
  there is no peer protocol, no listener on a network interface, no remote
  writer. Reach across projects goes through the hub, which knows where every
  store on this machine lives and can open any of them directly.

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
   **GNN** vector 0.4/0.6 (`src/graph/src/search.rs:65-66`); a node in only one index
   keeps that index's score. The GNN vector is what a background tick keeps
   re-embedding from graph structure.
2. **Expand** — walk reason edges out from the seeds
   (`src/retrieval/src/retrieval_expand.rs:167`) and return the traversal chain as provenance.
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

`kern daemon` exposes exactly one surface: a typed **`KernRpc`** (generated by
this repo's own `service!` macro in `src/transport/`) over a per-cwd local
socket — `invoke(name, args)` dispatching named operations that return plain
JSON. Every CLI verb with a routed counterpart (`query`, `get`, `ingest`
follow-ups, `forget`, `degrade`, `promote`, `graviton`, `claim-kind`, …) is a
thin dispatch to it; there is no separate protocol server to register
anywhere. Access control is socket ownership (path uid + `SO_PEERCRED`), not
a token.

A background **tick** (default 60s) drives decay, eviction, and clustering — an
idle daemon still maintains itself. A task that panics is caught, counted and
named rather than taking the loop down with it (`src/tick_loop/src/tick.rs:72`), so one bad
maintenance pass costs one task instead of every future tick; `health` reports
the panic and failure counts with the last of each. Persistence is **LMDB** (via
[heed](https://github.com/meilisearch/heed)) — an ACID, multi-process embedded
KV. Hot graph and cold tier live together in one LMDB environment
(`data.mdb` + `lock.mdb`) per data dir; vectors are stored int8, values are
`zstd(bincode)`. LMDB is single-writer: readers never block, writers serialize,
and a guarded-flush protocol keeps a stale in-memory snapshot from overwriting
newer on-disk state. HNSW, the GNN, beam search, and the RPC layer are all
written from scratch.

### The hub

One machine-level supervisor owns node lifecycle — and, since 2026-08-16, a
persistent registry of every kern on the machine. Clients resolve a project
root through the hub — auto-started when none runs (`[hub] auto_start = false`
opts out) — and the hub spawns the node, adopts an externally started one, or
hands back the live socket; a booting daemon registers its own root too, so
hand-started daemons appear as well. The registry
(`$XDG_STATE_HOME/kern/hub-roots.json`) survives hub restarts, and the hub
harvests per-kern stats (entities, kerns, store size) from live daemons.
`kern hub status` lists every known kern, loaded or cold, importance-sorted;
`kern query --all [--live]` fans one query out to every registered kern —
waking cold ones unless `--live` — and merges hits by score, each tagged with
its project root. `kern hub unload [root]` shuts one down gracefully
(save-then-exit over RPC). Nodes idle past `--idle-unload-secs` (default 30
min) are unloaded automatically and respawn on the next connect, so memory
tracks the active set, not the installed set. `kern hub merge <src> <dst>`
folds one project's graph into another (offline CRDT union; src untouched);
`kern hub stop` ends the hub, leaving nodes up. The data path stays direct
client→daemon — the hub is connect-time only. If the hub is disabled or
unreachable, everything falls back to the pre-hub behavior. Cross-machine
federation is explicitly out of scope: the hub only reaches stores this
machine already knows the location of.

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
curl -fsSL https://raw.githubusercontent.com/inner-zirkle/kern/main/scripts/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/inner-zirkle/kern/main/scripts/install.ps1 | iex
```

> Or build from source (needs a Rust toolchain): `cargo build --release` →
> `target/release/kern`, or `cargo install --path .`.

**2. Bring the daemon up.** There is no server to register anywhere — agents
use the `kern` CLI, and each verb dispatches to the project's daemon over a
local socket. `kern daemon` runs this project's daemon; on boot it announces
its root to the machine hub (auto-starting the hub unless `[hub] auto_start =
false`), so it shows up in `kern hub status`. Alternatively run the hub
(`kern hub`) and let `kern hub resolve [root]` spawn or adopt the node for a
project. A detached
child writes its stdout and stderr to an owner-only append log under
`<data_dir>/logs/` — `hub.log` for the machine hub, `daemon.log` for a node,
`.kern/data/logs/` by default — so a spawn that dies leaves a trace.
`kern status` reports whether a daemon serves this directory and whether
anything holds the writer lock; the offline admin commands (`reembed`,
`compact`, `gc`) refuse while it is held. With no daemon at all, the CLI
falls back to opening the store directly — guarded flushes keep a stale
snapshot from clobbering newer state.

**3. Wire the intake (optional).** kern is agent-agnostic: any tool that
writes a conversation delta to `<cwd>/.kern/intake/*` feeds the intake. Wire it
whatever way your client supports (a hook, a wrapper, or a manual `kern ingest`).
Recall needs no wiring — it is `kern query`.

**4. Know where the graph lands.** No config file is needed — every default
(embedding, reasoning, intake, tick) works out of the box against a local
Ollama. The daemon pins itself to the nearest ancestor holding `.git`, else the
nearest holding `.kern/`, else the launch directory
(`src/config/src/config.rs:197-208`), and creates `.kern/data/` there on open
(`src/store_core/src/lib.rs:322`) plus `.kern/intake/` for the drop dir. `mkdir .kern`
in a directory that has neither marker if you want the graph somewhere the walk
would not have chosen. (A `<cwd>/.kern/kern.toml` is only for overriding
defaults — see *Configure* below.)

**5. Seed the graph** (see *Seed the graph* below), then start a session. From
then on, store facts with `kern ingest` (or drop transcripts into
`.kern/intake/`), and pull them back with `kern query`. kern captures nothing
on its own — the writes are yours to make.

To verify it's working, run `kern health`. While a daemon serves, the routed
verbs (`query`, `get`, `forget`, `degrade`, `promote`, `graviton`,
`claim-kind`, `intake drain`) land in its live graph, so you are reading and
writing the serving state, not a stale disk copy.

kern is alpha: wire formats (the daemon RPC) change without migration paths.
Store formats are the one exception — a store written by the previous build
migrates one hop forward automatically on read (`kern migrate` rewrites it on
disk; `kern doctor` flags a store that hasn't been). A store more than one
format version behind is still rejected at load — wipe `.kern/data` and
reingest in that case.

### Configure

Configuration is **optional** — with no config file at all, kern ingests and
recalls with the defaults shown below. To override, create
`<cwd>/.kern/kern.toml` (project scope) or `<XDG_CONFIG>/kern/kern.toml`
(user scope).

The two scopes **deep merge per key** (`src/config/src/config.rs:356`): a project that
sets one field of a section keeps the user's other fields in that section.
Arrays and scalars are leaves — the project value replaces, never appends, so
`watcher.roots` is a complete list rather than an accumulator.
One deliberate exception: a scope that sets a section's `url` does not inherit
that section's `key` (`src/config/src/config.rs:447`). A cloned repo that redirects
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

[intake]
enabled = true          # self-learning (ON by default; set false to opt out)

[tick]
interval_secs = 60      # self-compaction cadence (0 = event-driven only)
```

The store stamps the embedding model and vector dimension that produced its
vectors and re-checks them on open and on every flush
(`src/store_core/src/lib.rs:447`). A swap is reported, never silently tolerated: the
`health` tool carries `embed_model`, `embed_dim` and `embed_mismatch`, and the
CLI prints `MISMATCH`. A query vector whose dimension disagrees with the
index returns no hits rather than nonsense, counted with a throttled log line
(`src/graph/src/search.rs:28`). An unstamped store adopts the configured model — that
is not a mismatch. `kern reembed` rewrites the vectors and stamps the model it
actually embedded with, not the configured one (`src/commands/src/commands_reembed.rs:69`).

### Intake & recall

kern is agent-agnostic. There is no client-specific plugin; both halves are
things any client can already do — write a file, run a CLI command.

- **Intake** — drop a conversation delta as a `.txt` file in
  `<cwd>/.kern/intake/`. The daemon drains it, distills typed claims out of it,
  and ingests them. Write the file however your client supports (a hook, a
  wrapper, or a manual `kern ingest`); kern only cares about the file.
- **Recall** — run `kern query`. It is relevance-targeted against the
  live graph and keeps provenance on every result.

Neither is gated on a pre-existing `.kern/`. A daemon pins itself to the nearest
ancestor holding `.git`, else the nearest holding `.kern/`, else the launch
directory (`src/config/src/config.rs:166-177`), and then creates the store and intake dirs
it needs. So any directory you run `kern` in gets a `.kern/` — that is the
cost of the CLI working everywhere, not something kern avoids.

**Requirements:** the `kern` CLI on `PATH` and a running embedding endpoint
(Ollama by default) for recall.

### Seed the graph

Once, from the project directory (while a daemon serves, `graviton` and
`claim-kind` route to it, so the writes land in the live graph):

1. Add a few gravitons — `kern graviton add <name> <text>` for each focus area
   the graph should gravitate around, e.g. *"decisions"*, *"project state"*,
   *"preferences"*. The text can be a one-line description or a full
   document/message — it is embedded whole as the graviton's pull vector. An
   optional `--mass` (default `1.0`) makes a graviton pull harder: ingest
   routes by `distance / mass`, and query ranking boosts thoughts near a
   graviton by `gravity_weight * mass * cos`. Memories that match no graviton
   land in `generic`; dense `generic` clusters auto-promote to new gravitons
   over time.
2. Optionally register extra claim kinds beyond the built-ins (`preference`,
   `decision`, `project`, `fact`, `code-fact`, `reference`, `procedural`) —
   `kern claim-kind add <name> <description>` once per custom kind; distillation offers
   registered kinds to the LLM alongside the built-ins.

After seeding, populate the graph with `kern ingest` during a session, or by
dropping transcripts into `.kern/intake/`. kern ships no session hook — the
write is always a caller's call.

### CLI commands

The agent surface and the human surface are the same binary. The daemon-side
operation set behind these verbs lives in `src/rpc/src/server.rs`
(`Server::invoke`); a verb with no serving daemon falls back to opening the
store directly.

| Command | Purpose |
| ------ | --------- |
| `kern query [--mode M] [--k N] [--all [--live]]` | Search the graph, LLM-free — the caller synthesizes. `--mode` weighs the signals (`vector` is the raw ANN probe of this store, read locally with no walk and no daemon); `--exclude-pending` drops rows a review policy still holds; `--all` fans the query out through the machine hub to every registered kern (waking cold ones unless `--live`) and merges hits by score, tagged per root. Routed to the serving daemon; superseded history and `as_of` time travel live on the daemon's `query` operation. |
| `kern ingest [--file F] [--retention-secs N]` | Add text. A dropped intake transcript is the other write path. |
| `kern get <id>` / `kern list` | One thought with provenance and edges (routed; prefix + cold tier resolved) / the on-disk kern tree. |
| `kern log [id] [--limit N]` | Git-shaped history. Bare: what entered memory or fell out of currency (added/superseded, newest first, from the bitemporal stamps). With an id: the revision chain — each revision with its source URI, created/invalidated stamps and the supersede reason as the why. Mutations that leave no stamp (forget, link, degrade) are not visible yet — that needs the planned operation journal (`docs/plans/GIT_SURFACE_PLAN.md`). |
| `kern link <from> <to> [--reason ..]` | Create a reason edge (LLM writes the reason if blank). Stored, walkable, shown in a result's chain. |
| `kern forget [ID \| --source scheme://object_id \| --match pattern] [--dry-run] [--force]` | Remove a thought, a whole source, or every case-insensitive text match, cascading edges. `--dry-run` previews a bulk removal. Facts are immune; `--force` (bulk only) is the one bypass. |
| `kern audit [--apply archive\|delete]` | Score stored content against noise/secret patterns; archive is reversible via `promote`. |
| `kern degrade <id>` | Punish a bad result: every edge incident on the thought decays, hardest first; below-threshold edges are removed. Entity-scoped. |
| `kern promote <id>` | Release a thought a review policy is holding. |
| `kern graviton {add\|list\|remove}` | Manage gravitons (named focus attractors): `add <name> <text> [--mass N]`. |
| `kern claim-kind {add\|rm}` | Register/remove a claim kind; registered kinds extend the built-in set distillation may emit. |
| `kern health` | Graph stats plus degradation signals: counts, tick queue depth and latency, task panics/failures, cold evictions, embed model/dimension with `embed_mismatch` — the serving daemon's numbers when one answers. |
| `kern intake {status\|drain}` | Intake queue report / one immediate drain pass (routed, so only one process ever drains). |
| `kern gc` / `kern reembed` | Reap empty kerns and compact the store / re-embed on a new model. Both refuse while a daemon holds the writer lock, naming the holder. |
| `kern export` / `kern import` | Versioned JSON of the hot graph out / CRDT-union merge in. |
| `kern doctor` / `kern repair <manifest>` | Read-only store diagnosis (flags a store still on the previous format, among other findings) / manifest-authorized fixes only. |
| `kern migrate` | Rewrite a store written by the previous format version to the current one in place (kern rows, cold tier, meta), one hop back only. Idempotent, writer-lock-guarded; reads already migrate in memory without this — running it rewrites the disk rows too. |
| `kern hub {status\|resolve\|unload\|merge\|stop}` | The machine hub: list every known kern (loaded or cold), resolve/spawn a project's daemon, unload one, merge two stores offline, stop the hub. |

Four verbs carry git-shaped aliases (both spellings work): `kern grep` =
`query`, `kern show` = `get`, `kern rm` = `forget`, `kern note` = `link`.

Every command reports failure the same way: the message goes to stderr as
`kern <command>: <what went wrong>`, stdout carries only the answer, and the
process exits non-zero — so a script can branch on the status instead of
grepping the text. An unreadable or invalid `kern.toml` exits 78 (sysexits
`EX_CONFIG`); everything else that failed exits 1.

The daemon additionally serves a few agent-only operations with no CLI verb of
their own (`events` — a read-only change feed cursor; `move`; `setup` — wiring
instructions for a calling agent), all through the same `invoke` dispatch.

---

## kern vs. traditional RAG

Traditional RAG is a pipeline you operate: chunk documents, embed them, stuff a
vector DB, and on every query do top-k cosine + prompt-stuff. kern is a memory
that operates itself.

| | Traditional RAG | kern |
| --- | --- | --- |
| **Ingestion** | Manual: you run a chunk-and-embed job over a corpus. | Caller-driven: an agent calls `ingest`, or a dropped transcript distills into typed claims via the intake — no re-indexing job. |
| **Unit stored** | Raw text chunks. | Distilled facts/decisions/preferences + *reason edges* between them. |
| **Retrieval** | top-k vector similarity. | Hybrid vector + BM25 with GNN-blended seeds, edge expansion, RRF + PageRank fusion, diversify — no LLM anywhere on the read path. |
| **Structure** | A flat bag of vectors. | A knowledge graph — a result carries the reason chain connecting it back to a seed, so recall shows *why* one fact reaches another. The chain is provenance, not score: an edge changes no ranking today (see *Retrieval* above). |
| **Growth** | Index grows unbounded; you re-index and prune by hand. | Self-compacting: heat decay + stigmergy GC + clustering keep the hot graph small; a capped cold tier preserves the recent tail. |
| **Staleness** | Stale chunks linger until you rebuild. | Cold, non-durable thoughts decay and evict on their own; Facts persist. |
| **Feedback** | None — a bad chunk keeps ranking. | `degrade` punishes a bad result's delivered score (entity-scoped, not path-scoped); access heat re-ranks what you actually use. |
| **Conflicts / sync** | Single store; multi-node needs external infra. | Single local store per directory, single-writer. Content-addressed ids and CRDT joins reconcile an external commit against an in-memory snapshot; there is no network sync to conflict over. |
| **Scope** | One global index. | One graph per working directory. |

The short version: RAG gives you **search over a corpus you maintain**. kern
gives you **memory that maintains itself** — it decides what is durable, forgets
what isn't, and stores the reason connecting one fact to another so a result can
show its chain instead of arriving as a bare nearest neighbor.

---

## Status

Intake, recall and self-compaction run today. kern is local memory: there is no
network layer and no remote writer, and the interconnection across projects is
the machine hub reaching stores it already knows the location of — registered
kerns are enumerable (`kern hub status`) and searchable (`kern query --all`)
machine-wide, and cross-machine reach is explicitly out of scope. Version
`2.0.0` — the first release under the format-compatibility policy in
[AGENTS.md](./AGENTS.md#format-compatibility): a store written by the
previous release migrates forward, it is not wiped.

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
