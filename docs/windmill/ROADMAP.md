# Roadmap — the single source of truth

State and work, one file. `FEATURES.md` says what exists, `CHANGELOG.md` says
what was decided, `VISION.md` says what "built" means. This file is the only
place that says **what is left**. Nothing else in the repo plans work.

**Ordering is the content.** The topmost item is the most important open thing in
the repo; importance falls monotonically from there. Rank is assigned by severity
× reach, with sequencing constraints as hard edges — where item B cannot be done
before item A, A sits above B and says so. A tier heading explains why its band
sits where it does; the heading is commentary, the position is the plan.

**Position is rank; the number is only a name.** They agreed when the list was
founded 1..85 and they stop agreeing the moment anything closes or arrives. Items
cite each other by number ("blocked on item 13"), so renumbering to restore the
match would silently repoint those references — the cure is worse. A closed item
retires its number rather than compacting the list, and a new item takes the next
free number wherever it ranks.

Stamped 2026-07-21, re-verified against source rather than against documents.
Tier 0 is gone: "what measures retrieval quality with no LLM in the scoring loop"
is answered, and `tests/e2e/` is the answer. That closure releases everything it gated
— items 32, 54, 55 and the whole of tier 8 are now judgeable the way item 86's two
candidate fixes were: apply, measure, keep only if `recall@1` holds.

A heading in this file was destroyed by an editing script that cut from an
item to the next one and swallowed the tier boundary between them; Tier 3 is
restored above its items. Say it here because a lost heading is invisible — the
items survive and simply appear to rank somewhere they do not. Tier 2 — "the
last cheap federation fix" — is a different absence and not a bug: item 16 was
its only item and closing it retired the whole band, exactly as Tier 0 went.
Tier numbers retire on close like item numbers do; a gap in the sequence is the
record, not a loss.

Context that is not work — north star, competitive position, non-goals, repo
laws, and what is closed — lives after the ranked list.

---

# Tier 1 — live defects on the default path that fail silently

These need no gossip, no flag and no unusual configuration. Every one produces a
wrong or missing result with no error, which is why they outrank both the
security work (armed only with federation on) and every feature.

### 100. `kern health` reports the serving daemon's degradations — closed 2026-07-22 `[surface]`

**Filed and closed 2026-07-22, found while reading item 28's close.** Eight of
the numbers `kern health` prints are scoped to the process that reads them: the
seven fail-open counters summed into `degraded:` are `AtomicU64` statics —
`query_dim_rejected` (`src/search.rs:10`), `below_floor_deliveries`
(`src/retrieval_score.rs:145`), `clock_skew_skips` and `unspilled_drops`
(`src/tick_stigmergy.rs:16`), `ingest_queue_refused` (`src/ingest_worker.rs:81`),
`ingest_dropped_chunks` (`src/ingest_worker.rs:319`), `remote_cap_dropped`
(`src/merge.rs:140`) — and `evicted:` reads `Store::cold_evicted`
(`src/base_store.rs:306`), an instance field every `Store::open` zeroes
(`src/base_store.rs:351`). `cmd_health` built all eight from
`graph_health_stats` over a graph *this CLI process* had just opened, and
`load_graph` (`src/commands.rs:281`) runs no search, no scoring, no tick, no
ingest and no merge, so the `if degraded > 0` branch was **unreachable from the
CLI** and `evicted:` was **always 0** however degraded the daemon was. Not
stale-and-sometimes-wrong: structurally zero.

Closed by preferring the wire. `cmd_health` awaits `daemon_health` once, up
front (`src/commands_admin.rs:43`), and hands the response to a new
`degradation_lines` (`:119`) beside `tick_health_lines` (`:172`). `HealthRes`
already carried all eight — `cold_evicted` (`src/transport/src/kern_rpc/dto.rs:42`)
and the seven at `:55`–`:67`, filled from the daemon's own `health_stats`
(`src/rpc/kern_rpc_server.rs:94`) — and all eight are now taken **whole**
(`src/commands_admin.rs:125`), not merged: a daemon is serving the store, so its <!-- docs-check: anchor-ok -->
counts are the true ones and this process's are noise about a graph nobody
queried. The local read stands only when nothing answers (`:138`), where zero is
honest. The no-daemon output is byte-identical to before, empty graph and
populated.

**The formatter is pure, so the tests are runner-independent.** These are
process statics, so a test asserting on `ingest_queue_refused()` passes under
`cargo nextest` (a fork per test) and fails under `cargo test --workspace
--locked` (one process, which is what CI runs) the moment another test
increments them — the trap item 92 hit. `degradation_lines` reads neither the
statics nor the store, only its two arguments, and its tests
(`src/commands_admin.rs:231`) construct both. The one that carries the decision
is `a_local_count_is_not_printed_over_a_serving_daemons` (`:259`) — local
counters nonzero, daemon healthy — which a `max()` or any additive merge fails.
End to end, `tests/e2e/test_health_surface.py` blinds the CLI's `data_dir` after the
daemon has opened its own, drains three claims the fake LLM refuses to embed
with a 400, and asserts `kern health` prints exactly 3: a count a blinded
process cannot hold and a constant cannot match.

Deciding behavior: verify-before-claiming — a health surface that cannot be
wrong is worth more than another counter on one that can.

### 9. Live writers: `ingest`/`link` still write locally `[surface]`

**Decided 2026-07-21, and implemented for every command the route fits.** The
choice named here was between routing the one-shot writes through
the daemon's RPC and teaching them to detect a daemon and refuse. Routing won,
for the reason the item already gave: refusing makes the CLI useless whenever a
daemon runs, which is always. `src/commands_route.rs` is that route —
`route(name, args)` probes `Endpoint::kern()` once, never spawns, and returns
`Done` / `Refused` / `NoDaemon`, so an absent daemon is the ordinary case and a
daemon that answers owns the write outright (a tool error is reported, never
retried against the store behind its back). `kern forget` and `kern degrade`
take it (`cmd_forget`, `cmd_degrade` in `src/commands_graph_ops.rs`); the local
path is the `NoDaemon` fallback and prints through the same two printers, so
the two paths cannot drift in wording.

The destructive entrance was already shut and stays shut: `src/lock.rs` is
an advisory writer lock over the data dir (std `File::try_lock`, no dependency;
MSRV 1.82 -> 1.89), held for the daemon's lifetime, and `reembed`, `compact`,
`gc` refuse while it is held and name the holder. `kern status` reports daemon,
hub and lock.

**What is left, and why it is not just more of the same.** `ingest` and `link`
cannot ride this route as it stands, because the RPC's only mutation surface is
`call_tool` — the *agent* boundary. `tool_ingest` clamps against `AGENT_SOURCE`
"regardless of what `p.source` claims" and `tool_link` writes
`MAX_AI_CONFIDENCE`, while `cmd_ingest` mints at `clamp_confidence(1.0, "user")`
and `cmd_link` at `1.0`. Routing them unchanged would silently demote every
CLI-minted Fact to an agent Claim. Routing them *with* their trust intact meant
putting a trust field on the socket — and a shared secret proves a uid, never
which of that uid's programs is talking, so any such field is declared, not
proven. **Restated 2026-07-22, after the item 18 removal decision:** kern
carries no caller identity, by decision, so the "wait for a provable
principal" exit no longer exists. The remaining choice is now binary and owed
here: route `ingest`/`link` and accept the agent-level clamp for routed
writes, or keep them local-with-guarded-flush (the status quo, which
`flush_guarded` already makes safe). Nothing is blocked on item 24 anymore.

**Closed 2026-07-21: `graviton add`/`remove` and `claim-kind add`/`rm`.** These
were the four shipped subcommands that reached `with_graph`
(`src/commands.rs:462` — load, mutate, `save_graph_unguarded`) with no routing at
all, so beside a running daemon each one wrote the WHOLE kern map back over
everything that daemon had committed since the CLI loaded. They needed no new
tool: the daemon already exposes `graviton` and `claim_kind`
(`src/mcp_tools_admin.rs`) with matching semantics. `graviton_at` and
`claim_kind_at` (`src/commands_admin.rs`) now route first and keep `with_graph`
as the `NoDaemon` fallback, printing through the same four printers so the two
paths cannot drift. The graviton add routes *before* it embeds — the daemon owns
the vector it stores, so embedding locally would be a second call to the same
model for nothing. Not blocked on item 24: neither command asserts trust or mints
a Fact, unlike `ingest`/`link`. Guarded by
`tests/e2e/test_graviton_routing.py`, which blinds the CLI's `data_dir` after the
daemon has opened its store and then makes the daemon flush again — the exact
sequence in which the graviton used to disappear.

**Closed 2026-07-21: `intake drain`.** It had no tool to route to, so it got one
— `intake_drain` (`src/mcp_tools_intake.rs`), one immediate pass of
`ingest::intake::drain_now` inside the daemon, returning `archived`. `drain`
(`src/commands_intake_cmd.rs`) routes first and keeps its in-process pass as
`drain_locally`, the `NoDaemon` fallback; both paths print through the same tail,
so only the archived count crosses the socket. This was a real race, not just
staleness: `drain_once` reads the queue directory and archives each entry, so a
CLI drain beside the daemon's poll loop distilled the same transcript twice and
raced the archive move. **The tradeoff, taken:** it puts a mutation on the
unauthenticated RPC socket. `gc` and `pulse` are already there, and `drain`
carries no caller-supplied content and no trust claim, so unlike `ingest`/`link`
it does not widen item 24's hole in a new way.

**Closed 2026-07-21: the standalone fallback.** `kern mcp`'s standalone server
was the last long-lived second writer, and the route could not save it — it has
no daemon to hand the write to, and a *sibling* standalone binds no socket, so
no probe can see one. Only the lock can. `claim_standalone`
(`src/commands_mcp_cmd.rs`) now claims the dir as `mcp-standalone` **before**
the graph is read and holds it for the process, and a claim that fails does not
boot a second writer: it spends one more attach window on the endpoint (the
usual holder is the daemon this process just spawned, late to bind) and proxies
to it, or exits 1 naming the holder. The tradeoff, taken deliberately: a
`kern mcp` that loses this race now gives its client no kern where before it
gave one that silently overwrote the other's graph. Availability was never the
thing at risk.

**Closed 2026-07-21: the read side.** `kern get` and `kern query` route before
they touch disk (`cmd_get` in `src/commands_graph_ops.rs`, `cmd_query` in
`src/commands_query.rs`), both over the existing `query` tool — `{id}` for the
detail read, `{text, mode, k}` for the ranked one. A serving daemon's live graph
answers; the local load is the `NoDaemon` fallback, and both paths render
through one printer (`print_detail`, `print_results`) over the tool's own JSON,
so wording cannot drift. One id resolver serves both
(`mcp::tools_query::entity_detail_by_id`), which is what stops a routed and a
local `get` disagreeing about what a prefix means.

Two things that were not free. The `k` had to be sent: the tool defaults to
`seed_k`, well under the pool the local path delivers, so routing without it
made `kern query` return 25 hits with a daemon up and 36 without, on the same
corpus — the hit count silently depending on whether something was serving.
`retrieval::score::delivery_cap` is now the one owner of that number and both
sides read it. And routing `query` sends the text to the *daemon's* embedder,
not this process's: correct, since it owns the index the query hits, but it
means `--embed-model` on a routed `kern query` is ignored rather than honoured.
That is the same "the daemon owns it" rule the write half took, and it is
stated here rather than discovered.

**`search` and `list` stay local, by decision.** Not oversight and not
deferred work: `search` is the raw-ANN probe with no matching tool behind it,
and `list` prints the on-disk kern tree. Both are what a developer reaches for
to inspect *the store*, and routing them would remove the only way to see what
is actually on disk while a daemon is up — which is also what makes them the
control in `tests/e2e/test_daemon_reads.py`.

**Neither read was a copy of the `forget` route, and the reason is the
constraint this item had been missing:** the daemon's tool surface is narrower
than the CLI's read commands, so a naive route trades staleness for lost
capability. `get` only became routable after `query{id}` was widened to match
what `cmd_get` resolves — a prefix, and the cold tier (`find_entity_by_prefix`
in `src/search.rs`, and the `cold_get` fallback both now reach through
`entity_detail_by_id` in `src/mcp_tools_query.rs`). `query` only became routable
after `tool_query` learned to return path chains, without which a routed
`kern query` would have silently lost its "--- Connections ---" section. Each
route cost a widening of the tool first; that is the shape of the remaining work
too.

`ingest` and `link` (routing decision owed above) still write the store directly;
`intake drain` joined the route 2026-07-21 once it was given a tool to route to.
Both are one-shot, so the exposure is a lost write rather than a lost graph —
and both direct-write paths are guarded. `cmd_ingest`
(`src/commands_ingest_cmd.rs`) and `intake drain`'s `flush`
(`src/commands_intake_cmd.rs`) retry through `persist::flush_guarded`;
`cmd_link` joined them 2026-07-21 via `link_and_persist`
(`src/commands_graph_ops.rs`) and `save_graph_guarded`, so a daemon committing
underneath any of them gets a refused flush and a reload rather than a clobber.

**Corrected 2026-07-21 — "one half remains" was wrong, and so was "the remaining
two unlocked callers".** The unguarded entry point is named
`save_graph_unguarded` and carries its precondition, and walking its call sites
turns up **three** classes, not two. The two this item already named stand and
still do not belong to it: `cmd_hub_merge` (`src/commands_admin.rs:1002`) writes
a destination graph it holds no lock on, and `maybe_self_heal_store`
(`src/commands.rs:433`) rewrites the store during boot recovery — hub merge
stops both daemons first, self-heal runs before the daemon serves, and neither
has been proven safe.

The third class *was* this item's unblocked half, and it is the one the closure
above discharged. `with_graph` (`src/commands.rs:462`) loads the graph, mutates
it and calls `save_graph_unguarded` (`:465`) holding no lock at all. `cmd_forget`
and `cmd_degrade` reach it safely, because they `route` first and only fall into
it on `NoDaemon` (`src/commands_graph_ops.rs:120`, `:444`). `kern graviton
add`/`remove` and `kern claim-kind add`/`rm` did not route at all, so beside a
running daemon they loaded a snapshot, mutated it, and wrote the *whole graph*
back over whatever the daemon had committed since — a full-graph clobber, not a
lost write, and the exact race the writer lock and the route exist to close.
**Corrected 2026-07-21 — this paragraph said "do not route at all" one commit
after that stopped being true.** All four now route first
(`graviton_at`, `src/commands_admin.rs:396`, route calls `:412` and `:458`;
`claim_kind_at`, `:518`, route calls `:526` and `:544`), keeping `with_graph`
`graviton` carries a name, seed text and a mass; `claim_kind` a name and a
description. Neither mints a Fact, so routing them widened item 24's hole no
more than `intake drain` did, and the tools they route to already existed with
matching semantics — `graviton` (`src/mcp_tools_admin.rs:87`, schema `:39`) and
`claim_kind` (`:161`, schema `:52`), both dispatched at `src/mcp.rs:193-194`.

So the item's remainder is **one** half, not two: `ingest`/`link`.
`graviton`/`claim_kind` were the unblocked half and are closed. The item does
not close on the read side alone; it closes when the route-and-clamp vs
stay-local decision above is made and recorded.

### 102. The GNN re-embeds the same corpus differently in every process `[retrieval]`

**Filed 2026-07-22.** It sits below item 9 because item 9 is the more severe
defect, and above everything after it because item 9 waits on a decision and
this is blocked on nothing. It is the first Tier-1 item that can be started.

**The symptom that found it.** `kern-gnn recall@1` measured 0.9028 and then
0.8611 across two runs of the *unchanged* suite, against a 0.8500 floor — 0.0111
of headroom, with nothing in flight touching retrieval.
`tests/e2e/test_gnn_recall.py` records its sample as "8 runs, 2026-07-22:
0.8889 - 0.9306" and sets the floor below the worst of it. 0.8611 is under that
recorded minimum, so the distribution the floor was drawn from is already
falsified by observation; the floor is not conservative, it is uncalibrated.

**Four sources, all read in source, three of them measured.**

1. **Weight init.** `GCNLayer::new` (`src/gnn_gcn.rs:20`) draws an unseeded
   `rand::rng()`, and `run_learned_propagation` builds both layers through it.
   Every seedable variant already exists and is used only by tests —
   `GCNLayer::with_rng` (`src/gnn_gcn.rs:24`), `LinearLayer::with_rng`
   (`src/gnn_layer.rs:31`), `Tensor::rand_with` (`src/gnn_tensor.rs:80`).
2. **Negative-edge sampling.** `sample_negative_edges`
   (`src/gnn_propagate.rs:147`) drew the same unseeded `rand::rng()`, so the
   link-prediction gradient trained against a
   different negative set every run.
3. **Snapshot node order.** `build_gnn_snapshot` iterates `kern.entities`
   (`src/tick_gnn_propagate.rs:71`), a `HashMap`, so `ids`, the feature-matrix
   rows and every index in `pos_edges` are per-process hash order;
   `kern.reasons` (`src/tick_gnn_propagate.rs:118`) orders `pos_edges` the same
   way.
4. **Index write-back order.** `apply_gnn_updates` iterates the `updates`
   `HashMap` (`src/tick_gnn_propagate.rs:212`) and inserts into `gnn_entity_idx`
   in exactly that order (`src/tick_gnn_propagate.rs:245`). HNSW links each new
   node to what is already present and takes its entry point from the first
   insert (`src/hnsw.rs:166`), so the GNN index's *topology* is per-process
   random even where the embeddings are not.

**Measured, not argued.** Two `run_learned_propagation` calls in ONE process over
one identical `GnnSnapshot` return different embeddings — `n0`'s first component
0.4227 against 0.2562, not a last-bit difference — and different marshalled
weights. Across three processes the `updates` iteration head is
`["n1","n23",…]`, `["n18","n17",…]`, `["n2","n12",…]`.

Sources 3 and 4 are item 29's defect exactly — "two hashed containers reached the
adjacency … Same corpus, different index, every process" — in a second place that
sweep did not reach. The remedy there was `BTreeSet` (`src/diskann.rs:123`).

**What it costs is not a wrong answer, it is an unfalsifiable one.** Every recall
number in this file measured through the GNN path is one draw from an unstated
distribution — item 28's, item 97's, and the floors inside
`tests/e2e/test_gnn_recall.py` itself. A gate under a nondeterministic measurement
reddens on the schedule of a coin rather than of a regression, and a green one
certifies nothing either.

**The fix, and the one part of it that is a decision.** Thread a seed: give
`GnnSnapshot` one, build the layers through the `with_rng` constructors that
already exist, hand `sample_negative_edges` an `&mut R`, and make both `HashMap`
walks sort by id. What is NOT decided here is where the seed comes from. A
constant makes two same-shaped kerns train identically; deriving it from the kern
id keeps kerns independent *and* keeps a rerun reproducible, which is the
recommendation and not yet a ruling.

**Success is a delta with a negative control.** `two_propagations_of_one_snapshot_are_bit_identical`
comparing `to_bits()`, and it must be shown FAILING with the seed reverted, or it
is asserting on a constant. These are pure functions over local state with no
process global, so `cargo nextest` and CI's `cargo test --workspace --locked`
cannot disagree about them. **e2e cannot be the gate here** — a nondeterministic
test that happens to pass proves nothing — but it is the confirmation
afterwards: once seeded, `tests/e2e/test_gnn_recall.py` must print the *same*
recall@1 on three consecutive runs, and only then can the floor be set to that
number instead of under a sample.

Deciding behavior: verify-before-claiming — a measurement that moves on its own
cannot certify anything, including that it is broken.

**Closed 2026-07-22 — and the ruling is neither of the two options above.**
`GnnSnapshot` carries a `seed` (`src/gnn_propagate.rs:59`); one `StdRng`
(`src/gnn_propagate.rs:80`) feeds `sample_negative_edges` and both
`GCNLayer::with_rng` calls, and all three `HashMap` walks are sorted
(`src/tick_gnn_propagate.rs:71`, `:115`, `:212`). The seed is derived from the
CORPUS — the sorted node ids, which are content hashes, streamed through SHA-256
(`gnn_seed`, `src/tick_gnn_propagate.rs:182`).

Why not the recommendation: a kern-id seed does not satisfy this item's own
title. `Kern::new_unnamed` / `new_named_child` fold `now_nanos` into the id
(`src/base_types.rs:524`), so the same corpus re-ingested into a fresh project is
a new kern, a new seed and a different embedding — measured, four e2e runs under
a kern-id seed printed recall@1 0.9306 / 0.8889 / 0.9167 / 0.9306. A constant
held three runs identical but hands every kern in the fleet the same initial
weights, so an initialisation that is bad for some graph shape is bad for all of
them at once with no diversity left to expose it. The corpus is the input the
title names and the only one with both properties.

That experiment also settled that there was no fifth source: under a constant the
whole suite — ingest, seed-index build, fusion, query — reproduced exactly, so
the kern id was the entire residual.

Both negative controls were run against the shipped tree, not asserted.
Restoring `rand::rng()` fails
`two_propagations_of_one_snapshot_are_bit_identical` on `n0` at 0.2173 against
-0.1046 — a sign flip, not a last-bit difference. Removing the two sorts fails
`two_identical_kerns_snapshot_in_the_same_order`, which builds two kerns the same
way in one process and gets two unrelated node orders (the specific permutation
differs per run, which is the point). `cargo nextest run --workspace` (994
passed) and `cargo test --workspace --locked` agree, as predicted.

The e2e confirmation ran and held: three consecutive runs printed recall@1
0.9028, recall@5 0.9583, MRR 0.9315, identically. `tests/e2e/test_gnn_recall.py` now
carries those values as the counts they are — `65 / 72` and `69 / 72`, because
the printed `0.9028` is a rounded-UP display of 65/72 and a floor pasted from it
fails against the run it was read from. The old floors and the "8 runs:
0.8889 - 0.9306"
sample are deleted rather than kept for comparison — they described a
distribution nothing draws from now, and one run of the unchanged suite had
already fallen through the bottom of it at 0.8611.

Note the 0.9028 is *below* several draws the old stochastic path produced. That
is not a regression: the old numbers were draws, this one is the value. Item 28's
and item 97's recall figures were measured through the unseeded path and remain
uncomparable to it.

---

# Tier 3 — the embeddable-endpoint track

kern's competitive claim is "everything a hosted service structurally cannot
do". An earlier cut of this track read "a hosted service serves *many callers*"
and grew a per-row ACL to chase that. That whole sub-track is **removed, not
deferred** — see the decision below.

### 18. Per-row ACL and user scoping — REMOVED 2026-07-22 `[surface]`

**Decision 2026-07-22: kern carries no per-row ACL and no user identity.** The
`Acl {scope, users, groups}` field on `Entity`, the `principals`/`scope`
arguments on the MCP `ingest` and `query` tools, `acl_admits` in
`matches_filter`, the `mcp/acl.rs` edge-endpoint verdict module (deleted with
the rest), the default-deny
resources surface, the ACL-aware dedup/supersede carries and the declared
`AuthReq::principal` were all removed wholesale, with no store or wire
compatibility kept.

**Why removed rather than finished.** The mechanism was structurally
unenforceable in this architecture, by its own record: ACL was caller-asserted
("the daemon cannot verify a caller's principals"), the socket principal was
declared and never proven, and same-uid callers are indistinguishable — no
shared secret can separate the CLI, the `kern mcp` proxy and the hub. Empty
`principals` meant *no filter*, so any caller could read everything by naming
nobody. That is a cooperative filter, not an access control, and it charged a
recurring tax: three read-surface bypasses were found and fixed after "done"
(cold-tier backfill, path chains, edge text), the per-edge endpoint verdict
cost a `find_entity` per edge and failed open on non-resident endpoints,
`Reason` never had an ACL of its own, and gossip shipped scoped rows ungated.
Every future read surface would have had to re-answer "does the ACL hold
here", and the history said the answer starts wrong.

**The model now.** kern is a single-trust-domain store, like SQLite or LMDB:
the process boundary is the access model — socket `0600`, `mcp-token` auth,
and the anti-squat checks (item 24's transport half, which stays). A host that
needs multi-caller scoping runs one kern per trust domain or gates its own
proxy in front; that isolation is *stronger* than anything kern could enforce
same-uid, which is the point.

**What re-adding would cost, named so nobody drifts into it.** A store format
bump, a gossip wire change, and re-litigating every read surface item 18's
history enumerated. Do not re-add fields "for later"; the tenant boundary is
the process.

### 20. Source-trust weighting `[retrieval]`

- ~~**A trust prior in the boost step.**~~ **Done 2026-07-21.** `apply_boosts`
  (`src/retrieval_score.rs:94`) now multiplies the composite by
  `RetrievalConfig::source_trust` (`src/config_retrieval.rs:55`), a map keyed on
  `Source::scheme()`. Empty by default, and an absent key is exactly `1.0`, so an
  unconfigured kern scores bit-identically. Post-fusion, as required — RRF is
  rank-based and a multiplier there would be meaningless.

**What remains is the part the item assumed and the data does not carry: an
`Entity` knows the CHANNEL it arrived on, never its AUTHOR.** `Source` is
`{File, Ticket, Session, Agent, Inline}` — a URI scheme. `kern ingest`, the
human path, writes `Source::Inline` (`src/commands_ingest_cmd.rs:63`), and the
MCP `ingest` tool's default writes `Source::Inline` too
(`src/mcp_tools_mutate.rs:207`). One tag, two trust principals: no weighting
keyed on scheme can separate a person from an agent, so `source_trust_user` and
`source_trust_agent` were not built — they would have been labels for a
distinction nothing records.

Nor does confidence stand in for it. `clamp_confidence`
(`src/math.rs:201`) caps a non-`USER_SOURCE` write at `MAX_AI_CONFIDENCE`,
which after `beta_params_from_confidence` (`src/ingest_place.rs:16`) is a 0.667
against 0.650 posterior — a 2.6% edge over an MCP agent, and since item 95 the
same 2.6% over the file watcher, whose `tag` is now its `source` `scheme`
(`src/ingest_file_watcher.rs:95`) instead of a raw `1.0`. So the item's
own headline — a user-authored claim outranking an auto-ingested one at equal
heat — is true by default at last, but only by 2.6%, which is a rounding error
rather than a trust model. `source_trust = { file = 0.8 }` is how to make it
mean something, and it is the channel it penalises, not the author.

**Author-level trust is retired under the item 18 decision (2026-07-22).** An
author principal on `Entity` is user identity, which kern no longer carries;
the consumer that wants author-weighted trust holds that mapping outside kern.
`source_trust` keyed on channel scheme is the whole trust surface. Do not add
`_user` / `_agent` knobs; they would read as working and weight nothing.

**Measurement half-closed 2026-07-23.** The active `source_trust` map was
invisible at runtime — an operator could not tell from `kern health` whether
it is configured. Now surfaced: `Server::health_stats` (`src/mcp.rs`) JSON
carries `source_trust` from `self.cfg.retrieval.source_trust`; `trnsprt::HealthRes`
gains `#[serde(default)] source_trust: BTreeMap<String, f64>` (old daemon →
empty, same shape as `gini_access`/`max_kerns`/`preset`); `kern health` prints
`source-trust:` daemon-sourced only (item 100 rule), empty map → `(none)`,
no daemon → no line; `kern://local/health` by construction. Proved by
`kern_health_prints_source_trust` (`file=0.8,ticket=0.9`; empty → `(none)`;
no daemon → empty) + dto round-trip `source_trust {file:0.8, ticket:0.9}` +
old-payload absence → empty (standing negative-control guard). Existing
`kern_health_*` + dto tests green unedited. `cargo test -p kern --lib` 964
passed, 0 failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed. Decided
by fix-the-root (surface the configured map, do not tune it — author-level
trust is retired under item 18), name-the-tradeoff (one map — the channel
scheme; the `_user`/`_agent` labels the item retired stay retired), verify-
before-claiming (old-payload-absence guard). See the 2026-07-23 CHANGELOG
entry.

### 21. The review lifecycle has a caller-facing surface: `promote` and `exclude_pending` — closed 2026-07-22 `[surface]`

**Corrected 2026-07-22 — "three of four parts landed" counted a part no caller
can reach.** Two parts landed and are reachable. `ReviewState` is on `Entity`
(`src/base_types.rs:284`) behind a `FORMAT_V7` bump — old stores are rejected
rather than defaulted, per the `FORMAT_V6` precedent, pinned by
`decode_rejects_older_version_bytes`. Source-level policy is
`IngestConfig::review_policy` (`src/config_ingest.rs:17`), keyed on source scheme
with unknown schemes rejected at config load (`:33`), resolved once at the ingest
gate by `review_for` (`src/ingest_config.rs:13`). Both work.

**The third part was engine-only, and re-verified as such before any code was
written.** `exclude_pending` was already a real `QueryOptions` field
(`src/retrieval_score.rs:49`) and a real predicate (`:215`) that also makes
`is_active()` true (`:63`) so the filter takes the pre-filtered ANN path — but
**nothing outside `src/` could set it to `true`.** It was absent from `QueryArgs`
and from the `query` tool's schema `properties` (both `src/mcp_tools_query.rs`),
and there was no CLI flag; walking every `exclude_pending` in the tree found
exactly one write, in its own unit test (`src/retrieval_score.rs:498`). So the
hold half was as unusable as the release half, for the same reason — the state
was decided entirely inside the process. That is why this was one slice:
`promote` alone would not have made the feature usable, because a host that
promoted a row could still never have filtered it.

**The default is `Active`, deliberately.** Pending-by-default would have made
every existing ingest path silently non-retrievable — a behaviour change
disguised as a schema addition, and one that craters recall rather than failing
loudly. Active-by-default means the feature does nothing until a host opts in,
which is the correct direction for a filter that can hide data.

**Both halves shipped 2026-07-22, as one slice.** The release half is a
`promote` tool (`src/mcp_tools_mutate.rs`) with a dispatch arm in the single
`match name` and a `kern promote <id>` subcommand routing through `route()`,
which is generic on the tool name (`src/commands_route.rs:10`) — so this cost a
dispatch arm and a subcommand, not a new route, exactly as predicted. Both the
tool and the CLI's no-daemon fallback go through one `graph_ops::promote_entity`,
so the routed and local writes cannot disagree about what "reviewed" means. It is
idempotent (`promoted: false` on an already-active row) and loud on an id nothing
resolves — a silent success there would tell a curator a claim was released while
it is still held. The hold half is `exclude_pending` on the `query` schema
(`properties`), on `QueryArgs`, carried by `build_query_options`, and behind a
`kern query --exclude-pending` flag.

**A third unreachability was found while building, one layer below the two this
item recorded.** `[ingest] review_policy` could not be set from a `kern.toml` at
all: `Config::load_with_user` refused the whole `[heat]`/`[ingest]`/`[retrieval]`
tables as preset-managed, so the policy that decides what is held was settable
only from inside the process — the same defect as `exclude_pending`, one level
down, and it made the e2e impossible. Fixed at the root rather than worked
around: what a preset owns is TUNING, and `Preset::apply` writes exactly one key
in that table (`ingest.dedup_threshold`). `[ingest]` now accepts `review_policy`
and nothing else; `[heat]` and `[retrieval]` are untouched, and a tuning knob
smuggled in beside `review_policy` is still refused. Both directions are pinned
by `a_real_kern_toml_can_set_review_policy_and_nothing_else_in_ingest`.

**The tradeoff, named.** `promote` lands on the authenticated socket, and any
caller holding the graph's `mcp-token` can release a held claim — the process
boundary is the whole authority model, per the item 18 removal decision. Taken
because the alternative was shipping neither half, and a host that enabled
`review_policy` would strand every claim it held.

**Coverage.** This is e2e-measurable, and it is measured: `tests/e2e/test_review_lifecycle.py` runs the whole loop — policy holds an
`inline` ingest, `query --exclude-pending` misses it, `kern promote` releases it,
the same query returns it — twice, once against a serving daemon blinded the way
`test_graviton_routing` blinds it (so the release provably landed in the daemon's
live graph and survived its persist) and once with nothing serving, for the
`NoDaemon` fallback. `tests/e2e/conftest.py`'s `write_config` grew one `review_policy`
kwarg, emitted only when set so every other test's config text is byte-identical.

Original text, kept for the record: `ReviewState` on `Entity` (added with a store
format-version bump — alpha rejects old stores rather than defaulting them) +
source-level review policy in config + an `exclude_pending` query filter and a
`promote` tool. Lets a host hold auto-distilled claims out of retrieval until a
human curates them. Requires 18's `QueryOptions` work first — review filters are
more `matches_filter` predicates. *(The sentence "No `ReviewState`,
`exclude_pending` or `promote` exists in `src/`" was true when filed and is now
wrong about the first two.)*

### 24. RPC socket authentication `[surface]`

**Mostly closed 2026-07-22, and deliberately left open — read the residues at
the bottom before citing this as a blocker.** The socket authenticates the
connection. It cannot tell one same-uid caller from another, and by the item 18
removal decision (2026-07-22) it never will: kern carries no caller identity,
so the token gate plus the `0600` socket *is* the whole access model.

**Narrowed 2026-07-22 — "anything that can open the path" was already false on
Unix when this was written.** `harden_socket`
(`src/transport/src/typed/local.rs:355`) sets the socket `0600` on both the fresh
bind and the stale-rebind path, pinned by `a_bound_socket_is_owner_only` and
`a_rebound_stale_socket_is_also_owner_only`, so a foreign uid never reaches it
and the only residue is the sub-ms bind→chmod window item 84 already carries.
No document said so — `FEATURES.md` §13 still read "anything that can open the
path", which is why this is recorded here rather than assumed.

**Built 2026-07-22 — the connection is authenticated.** One `AuthReq` frame
(`src/transport/src/kern_rpc/auth.rs:79`) carrying the graph's `mcp-token` — the
same secret the HTTP surface already demands (`resolve_mcp_token`,
`src/config_serve.rs:64`), never a second one — is compared in constant time
before anything dispatches. The ordering is structural, not remembered:
`serve_authenticated` (`src/rpc/kern_rpc_server.rs:177`) builds the handler
*inside* a closure that only runs after the verdict, so on a refused connection
no handler exists for a method to reach. Every non-match returns `Err`,
including an empty `expected` — a daemon that cannot read its secret serves
nobody rather than everybody — and `run_server` resolves the token before it
binds, so that state is unreachable in practice as well as harmless. Windows
gets the same posture the Unix `0600` states: an owner-only SDDL,
`D:P(A;;GA;;;<user>)`, built from the process token's own SID
(`src/transport/src/typed/local.rs:392`) and passed to *every* pipe instance — <!-- docs-check: anchor-ok -->
the `accept`-created ones too (`:641`), since an instance created without it
would be a hole beside a locked door.

**The tradeoff that was taken, named.** The secret proves *a uid*, not *a
program*. The CLI, the `kern mcp` proxy an agent drives and the hub all run as
the same user and can all read the same file, so no shared secret can separate
them. The declared `AuthReq::principal` field that used to ride the frame was
removed with item 18 — it was recorded and never consulted, a label that read
as identity and was not.

**Why it stays open — residues, none of them "the gate might not hold".**
The gate holds: making verification always succeed fails the no-token and the
wrong-token tests, and gutting the byte compare fails them too (both mutations
re-run 2026-07-22). What is left is everything the gate does not cover.

1. **Windows is unexecuted.** The descriptor typechecks — `cargo check --target
   x86_64-pc-windows-msvc -p trnsprt` is clean, and a deliberate type error
   inside the `cfg(windows)` module does fail it, so that is not a vacuous
   pass — but no line of it has ever run. The SDDL is unparsed, the token query
   unmade, and there is no Windows test: `bind_tests_unix` is
   `#[cfg(all(test, unix))]` and stays that way. Treat the pipe as *believed*
   owner-only, not *known* to be.
2. **The socket secret is the HTTP secret, and the socket path is squattable.**
   With no `XDG_RUNTIME_DIR` the endpoint falls back to `/tmp/kern-<tag>-<user>.sock`
   (`Endpoint::scoped`, `src/transport/src/typed/local.rs:44-55`), and `/tmp` is
   sticky-but-writable: another local user can bind that name first. On the
   socket the stolen token buys nothing (the real socket is `0600`), but it is
   the *same* token `mcp_addr` demands, so a socket-side disclosure is an
   HTTP-side compromise. That is the cost of reuse, and reuse was still the
   right call — a second secret is a second thing to mint, rotate and get
   wrong.

   **The disclosure is closed 2026-07-22 — the client now authenticates the
   server, twice.** `require_owned_by_caller`
   (`src/transport/src/typed/local.rs:237`) stats the endpoint and refuses unless
   both the name and what it resolves to are owned by this euid.
   `require_peer_is_caller` (`:283`) then reads `SO_PEERCRED` off the connected
   socket and refuses unless the process serving it is this euid. Both sit in
   `connect_kern` (`:314`), which returns before
   `present_auth` (`src/transport/src/kern_rpc/client_local.rs:44`) writes the
   token — so both are ahead of every byte a client could send, and a check
   after frame 1 would be decoration. It fails closed: any stat error, a
   dangling symlink, and an unreadable peer credential all refuse.
   `Endpoint::hub()` is the same `scoped()` name and reaches the wire through
   the same `connect_kern`, so the hub socket is covered by construction rather
   than by a second check.

   **Why two checks and not one.** The stat is the cheap one and it is not
   sufficient on its own: it describes a *name* at one instant, and the window
   between it and the `connect` is opened by our own daemon rather than by an
   attacker — `Drop for LocalListener` (`src/transport/src/typed/local.rs:654`)
   unlinks the socket on every shutdown and the stale-rebind path unlinks it
   too, so a name that stats as ours can be free a microsecond later and rebound
   by somebody else before the `connect` lands. Waiting for a daemon restart is
   not a privilege an attacker has to earn. `SO_PEERCRED` is the fact the kernel
   recorded when the peer called `listen`, so no rename can move it; the stat is
   kept in front of it only because refusing before opening a connection gives a
   message that names the squatter's uid. **Do not describe the stat alone as
   sufficient** — an earlier draft of this entry did, and it was wrong.

   **Both checks are mutation-tested (re-run 2026-07-22).** Neutering
   `require_owned_by_caller` to `Ok(())` fails 6 of 6 targeted tests, including
   `a_foreign_owned_endpoint_is_refused_before_the_token_is_presented` — the
   ordering assertion, which holds because connecting to a root-owned path
   fails either way but as `UntrustedEndpoint` with the check and `Io` (EACCES)
   without, so it cannot pass for the wrong reason. Neutering the peer uid
   comparison fails `the_peer_check_reads_the_server_uid_and_decides_both_ways`.
   That test injects the expected uid rather than reading `geteuid()`, because
   a socket bound by a second uid is not something a test can create.

   **What closed and what is still owed, in order of how much it matters.**
   - **CLOSED 2026-07-22 — the bind path refuses instead of standing down.**
     The `AddrInUse` arm (`src/transport/src/typed/local.rs:513-540`) now runs
     the same two checks `connect_kern` does, in the same order:
     `require_owned_by_caller` on the name, then `UnixStreamAdapter::connect`
     and the peer check on whatever answered. Either refusal returns the new
     `BindError::Untrusted` (`:335-346`) carrying the foreign uid, never
     `BindOutcome::AlreadyRunning`, and — the half that was not recorded
     before — never reaching the `remove_file` below it. That unlink was the
     wider bug: it ran on a path nobody had verified, and while `/tmp`'s
     sticky bit refuses it for a foreign *socket*, it does not protect a
     *symlink*, so a link this uid owned pointing at a foreign target was
     unlinked and rebound.

     **Fail-closed, traced rather than asserted.** There is exactly one
     `remove_file` in the function (`:536`) and it sits inside the `Err(_)`
     branch of the connect, past the `?` on `require_owned_by_caller`, so it
     is unreachable on a name that has not been proved ours. The three error
     shapes were run: a dangling symlink refuses and the link survives, a
     vanished path refuses (the `Io(NotFound)` the predicate returns for
     absence is mapped to `Untrusted` here, because in *this* arm the kernel
     just said the name was taken), and a plain file this uid owns is still
     reclaimed. **The tradeoff that buys:** a name that disappears between the
     `EADDRINUSE` and the stat — a predecessor exiting mid-race, whose `Drop`
     unlinks — now refuses where it used to rebind. Fail-closed was chosen
     over a retry, and the operator sees the reason because the refusal
     prints; a planned handover does not come through here at all, it comes
     through `adopt_kern_listener`.

     **Reclaiming our own stale socket still works**, and not only per the
     mode tests: verified against a real second process — a same-uid,
     different-exe daemon bound the path, read as `AlreadyRunning` while
     alive (`SO_PEERCRED` proves a uid, not a program), was `SIGKILL`ed, and
     the next bind reclaimed the leftover socket and hardened it to `0600`.
     Pinned in-suite by `a_stale_socket_file_is_removed_and_rebound`,
     `a_bound_socket_is_owner_only` and
     `a_rebound_stale_socket_is_also_owner_only`.

     **The refusal is visible at both call sites**, which is the point of it:
     `run_hub` (`src/hub_serve.rs:305`) already `eprintln!`ed its `Err` arm;
     the daemon's (`src/commands.rs:854`) only `tracing::error!`ed, so a
     refusal there would have printed nothing at the default level while the
     `AlreadyRunning` arm beside it printed to the terminal. It now does both.

     **Mutation-tested 2026-07-22, and both checks are caught.** Neutering
     `require_owned_by_caller` to `let _ =` fails
     `a_symlink_to_a_foreign_target_refuses_the_bind` — the test built on the
     `foreign_path()` helper, which lives in `owner_tests_unix` (not
     `bind_tests_unix`, as an earlier draft of this entry said) and is now
     `pub(super)` so the bind tests share it rather than growing a second
     copy. Neutering the peer check fails
     `a_live_endpoint_served_by_another_uid_refuses_the_bind`. Reverting the
     whole arm to its pre-change body fails both and leaves the other four
     bind tests green, so neither test is a tautology.
   - **The peer check's wiring is covered in the bind arm and not in
     `connect_kern`.** ~~It was recorded here as untestable on one uid; it was
     untested.~~ **Closed 2026-07-22 — `connect_kern` now has the seam too.** A
     `#[cfg(test)]` `connect_kern` path injects the expected uid (mirroring the
     bind arm's `bind_unix(path, expected_peer)`), and
     `connect_kern_refuses_when_the_peer_uid_differs` drives it with
     `geteuid().wrapping_add(1)` against a socket this uid serves, asserting
     `AdapterError::UntrustedEndpoint` naming `served by uid {euid}`. Negative
     control (neuter `require_peer_uid` → foreign uid accepted → test reds,
     green on revert) — same mutation the bind arm test is mutation-tested with.
     `cargo test -p trnsprt` 61 passed (+1). The owner-check half
     (`require_owned_by_caller`) was already covered by
     `connect_kern_refuses_a_foreign_endpoint_before_it_connects` via a
     root-owned `foreign_path()`; this closes the peer-uid half — the one gap
     this entry named.
     The arm is reached through `bind_unix(path, expected_peer)`
     (`src/transport/src/typed/local.rs:507`), split out exactly as
     `require_peer_uid` is split out of `require_peer_is_caller` and for the
     same reason, so a test drives the whole arm against a real socket and a
     real `SO_PEERCRED` read with a uid that is deliberately not the server's.
     The two alternatives considered do not bite: a same-uid child with a
     different exe is *correctly* accepted, because the check claims a uid and
     nothing finer (measured above), and an abstract-namespace socket or a
     `socketpair` has no filesystem name, so a path bind never returns
     `EADDRINUSE` for one and the arm is never entered.
   - **The live squat end to end is still not executable.** A real foreign
     daemon holding the real path needs a second uid. What is proven is the
     symlink shape, the verdict, and the arm's control flow; what is not is a
     genuine cross-uid squat.
   - **Windows gets no analogue and needs none** — a named pipe has no owning
     uid, and the server side already pins every instance to this process's
     SID, so both checks are `cfg(unix)`.
   - **e2e measures only half of this, and cannot measure the other half.**
     `tests/e2e/conftest.py` sets `XDG_RUNTIME_DIR` per test, so e2e always takes the
     XDG path and never the `/tmp` fallback that is the vulnerable one, and it
     has no second uid with which to create a foreign-owned socket. So
     `tests/e2e/test_daemon_reads.py` and `tests/e2e/test_graviton_routing.py` pin exactly
     one thing — that the checks did not break connecting — and the refusal
     itself is unit-test territory. The bind half added 2026-07-22 is covered no
     better: same `XDG_RUNTIME_DIR`, same single uid, so e2e confirms only that
     the daemon still binds and serves, never that it refuses. Do not read a
     green e2e run as evidence that the squat is covered.
3. ~~**The pre-auth frame is unbounded and untimed.**~~ **Closed 2026-07-22 by
   item 98.** This entry read the defect correctly — per connection rather than
   an accept-loop stall, and a cap belonging in `decode` — and named the one
   thing it could not settle: the number. `verify_auth`
   (`src/transport/src/kern_rpc/auth.rs:98`) now bounds that read at 1 KiB and 5 s
   and lifts both once the frame is in hand, so `call_tool`'s whole documents
   still travel the framing they need
   (`JsonEnvelopeCodec::decode`, `src/transport/src/typed/codec.rs:53`). The
   ranking stands as written: `0600` means the caller was already same-uid, so
   this was a robustness bound, not a boundary failure.

The item's second half is **retired 2026-07-21 — verified
false**: `KernRpc` does not mirror MCP 1:1 and never did. The contract is four
methods (`health`/`shutdown`/`call_tool`/`list_tools`,
`src/transport/src/kern_rpc/svc.rs`) and every tool reaches the daemon through the
one `call_tool` passthrough, so there is no second dispatch copy to drift
against repo law 3 — as the closed list already recorded while this item claimed
the opposite.

---

# Tier 4 — scaling cliffs

None of these is wrong today. Each converts "works on my corpus" into "does not
work at 10×". Item 1's instrument now exists, so a fix here is judgeable the way
tier 8 is — but only at the corpus size `tests/e2e/` builds, which is small: the cliff
itself stays unmeasured until something generates a large one. Latency is the
half the harness can already claim.

### 25. O(N) importance scan per retrieve `[retrieval]`

`seed_important` (`src/retrieval_seed.rs:131`) iterates `g.all()` ×
`kern.entities`, called unconditionally once per retrieve
(`src/retrieval_query.rs`, in `retrieve_profiled`).

**Narrowed 2026-07-21, after item 26 landed.** The scan is 1.9–3.9× faster and
its share of retrieve is roughly halved, but it is still O(N): what this pass
removed was a parallelism bug, not the linear walk. The index the item asks for
turned out to be blocked, and the blocker is named below so the next attempt
does not rediscover it.

**The old tables are replaced, not amended — both were measured before item 26.**
Re-measured with the same instrument (`tests/seed_scale.rs`, release, 20 reps per
configuration), the PageRank comparison inverts. PageRank is now ~2–3 ms rather
than a flat ~20 ms, so the scan is the larger cost everywhere above 1%
eligibility:

| N=100k | scan | PageRank | scan ÷ PageRank | scan as share of retrieve |
| --- | --- | --- | --- | --- |
| 1% eligible | 2.37 ms | 2.46 ms | 1.0× | 39.7% |
| 10% eligible | 6.74 ms | 2.93 ms | 2.3× | 60.1% |
| 50% eligible | 18.82 ms | 2.08 ms | 9.0× | 72.1% |
| 100% eligible | 35.69 ms | 2.54 ms | 14.1× | 71.2% |

The superseded row "100k, 1%, 12.3% of retrieve" is not a contradiction: the
scan cost the same, but retrieve no longer carries PageRank's 20 ms, so the same
absolute cost is a far larger share. The old item said the numbers would decide
the ordering; they have.

**Fixed here: the scan was never actually parallel on the ordinary corpus.**
`kerns.par_iter().flat_map_iter(...)` parallelised over *kerns* and walked each
kern's entities on a single thread, so a one-kern graph — what `tests/e2e/` builds and
what a fresh install is — scanned the whole corpus serially however many cores
were free. This item's own "Rayon-parallel" claim was wrong in exactly the case
that matters. The inner walk now splits too
(`src/retrieval_seed.rs:177`), on 8 cores:

| N=100k | before | after | |
| --- | --- | --- | --- |
| 1% eligible | 2.37 ms | 1.24 ms | 1.9× |
| 10% eligible | 6.74 ms | 1.73 ms | 3.9× |
| 50% eligible | 18.82 ms | 8.19 ms | 2.3× |
| 100% eligible | 35.69 ms | 12.04 ms | 3.0× |

Recall is exactly unchanged (0.9306 / 0.9722 / 0.9471). The selection is
bit-identical rather than merely equivalent —
`parallel_importance_scan_equals_the_sequential_scan_it_replaces` compares ids,
rank positions and score *bit patterns* against an independently written
sequential gate over a generated three-kern graph. At N=10k the change is inside
the noise floor of a box running three worktrees, and it buys nothing there;
the win is a large-corpus win.

**The index is blocked on the absence of an entity-mutation chokepoint, and
`mutation_epoch` is not one.** `bump_mutation_epoch`
(`src/graph.rs:439`) has exactly three callers, all inside `graph.rs`:
`get_mut`, `register`, `deregister`. But `GraphGnn::kerns` and `Kern::entities`
are public fields, and ~20 non-test sites mutate them directly without passing
through any of the three — `merge_remote_entity` (`src/merge.rs`) inserts a
fresh Fact, `reembed` replaces every vector through `values_mut`, gossip writes
phantom-kern entities, clustering moves entities between kerns. Each of those
changes an input to the importance gate (`has_vector`, kind, `access_count`)
while leaving the epoch untouched.

Worse, the one mutation that *creates* importance is epoch-silent on purpose:
`commit_access_ids` (`src/retrieval_score.rs:326`) stamps access on every
delivered result and deliberately bypasses `get_mut` so it will not invalidate
the semantic query cache (`src/retrieval_score.rs:360`). An eligible-set index
keyed on the epoch would never see a Claim cross
`important_access_threshold` — stale forever, in the direction that silently
drops seeds and moves recall with no error anywhere.

That is verified rather than argued: installing a `mutation_epoch`-keyed memo
over `seed_important` makes
`an_eligibility_change_is_reflected_with_no_epoch_bump` fail on "crossing the
access threshold makes an entity important on the very next retrieve". The test
is left behind as the guard, so the next attempt at an index fails loudly
instead of shipping the regression quietly.

**What is left is therefore a different question: what makes entity mutation
observable?** Two candidates, neither cheap. Make `Kern::entities` private behind
an accessor that versions its kern — correct by construction, but mechanical
across ~40 call sites in `ingest/`, `tick/`, `gossip/` and `commands/`. Or
hand-maintain the eligible set at each mutation site the way `entity_idx`
already is — the convention the codebase actually uses, and the reason
`merged_remote_entity_is_vector_searchable_without_rebuild` exists — at the price
of a fifth thing every mutation site must remember. Not decided here.

The costs to weigh when it is: an eligible-id set is ~80 B per eligible entity
(a `String` plus its set slot), so ~8 MB at N=100k fully eligible, and it puts
O(1) index maintenance on every write plus a full O(N) rebuild whenever the
freshness signal is lost. An index that slows ingest to speed up query is the
trade, and it only pays where eligibility is low — a corpus where everything is
eligible cannot be helped by an index at all.

**Guard added 2026-07-23 — the four named non-access mutation sites now pin
epoch-silence.** `non_access_mutations_leave_mutation_epoch_unchanged`
(`src/retrieval_seed.rs`) snapshots `g.mutation_epoch()` before and asserts it
unchanged after each of the four sites named above:
`merge_remote_entity` (fresh Fact into phantom kern), reembed `values_mut`
(vector in place), gossip `inject_remote_scope`/`new_phantom_kern` (phantom-kern
insert), and `do_cluster` `move_entity` (entities between kerns). Same
item-77 shape as the existing `an_eligibility_change_is_reflected_with_no_epoch_bump`
guard for the access site. Negative control (add `g.bump_mutation_epoch()` at
one site → the corresponding sub-assert reds, `right: 0` epoch 0→1, green on
revert) — the guard bites on a real bump, so a future chokepoint fix that bumps
the epoch at one of these ships loudly instead of a half-bumped index.
`cargo test -p kern --lib` 959 passed, 0 failed, 4 ignored. Decided by fix-the-
root (pin the invariant, do not build the index — the chokepoint decision is
owed separately), name-the-tradeoff (four representative sites, not the full
~20 — the rest are the same class), verify-before-claiming (negative control).
See the 2026-07-23 CHANGELOG entry.

### 26. PageRank allocates four N-sized buffers on every query — closed 2026-07-22 `[retrieval]`

**Narrowed 2026-07-21, narrowed again 2026-07-22, closed 2026-07-22.** The flat
per-query cost went first, then the full-reach regression that replaced it, and
last the allocation the title names. Each stage is kept below so the closed parts
are not re-opened and the numbers that are now stale are marked as such.

**Measured before, `tests/seed_scale.rs` in release, default minus
`pagerank_enabled: false`.** At N=100k it cost a flat **~18 ms per query** —
18.8 / 17.9 / 18.1 / 17.1 ms at 1 / 10 / 50 / 100% eligibility. Flat was the
finding: the cost did not shrink when the query filtered hard, because the power
iteration walked the whole adjacency regardless of how few entities survived.

**Measured after, same harness.** 1.7 / 3.1 / 1.9 / 1.3 ms at the same four
points; a filtered retrieve went from 24.0 ms to 7.1 ms end to end. Three
post-change runs put every one of those points in a 1.3–6.0 ms band — the box
has two sibling worktrees building into the same target directory, so ±2 ms is
the noise floor and no single point in that band should be read as exact. The
before numbers do not need the same caveat: they were 17–19 ms at all four
points, which is outside it. The walk is now confined to the teleport support
and what it reaches
(`src/retrieval_pagerank.rs`), which is exact rather than approximate — every
node outside the reached set holds a literal 0.0, so every term the full-width
loop added for it was `+0.0`. `confined_iteration_equals_the_full_width_one_bit_for_bit`
compares the two against each other on bit patterns, not tolerances.

**The item's own proposed closure is unavailable, and this is why.** It read
"the scores depend on the graph, not the query, so persist the vector and
recompute on a tick". They do not: the teleport vector is personalised at the
query's dense and lexical seeds, so a per-graph vector is *global* PageRank —
already weighed and already rejected on the site, "popular entities top every
query, relevant or not". A cache of it would not be a faster version of this
feature, it would be the alternative the feature exists to avoid. Nothing about
cold start or persistence is decided here because there is nothing correct to
persist: personalised PageRank is linear in the teleport vector, so the only
exact cache is one basis vector per seed node, which is O(N) memory apiece and
~75 misses on a cold query. A cache keyed on the seed set instead would report a
large win on any harness that repeats a query — which is the only kind of harness
we have — while doing nothing for real traffic.

**The full-reach regression is closed 2026-07-22, and it did reproduce.** The
cost tracked reach, so a query whose seeds reached the whole graph paid the whole
graph — and paid it through a list where the loops it replaced vectorised over a
slice. Rechecking the old instrument (`cost_against_full_width_by_fanout`) on
this box put out-degree 16 at 31.8 ms against 26.7 ms rather than the 37.7 / 26.3
recorded above; the direction held, the exact 1.4× did not, and that instrument
was never a fair one — it charged its full-width reference a 100k-row sort the
confined path does not pay.

The fair instrument is new: `cost_against_full_width_by_reach` in
`src/retrieval_pagerank.rs`, ignored by default. It confines every edge to a
contiguous block of known size, so the seeds' reach is the block while the graph's
edge count and per-node out-degree do not move with it, and it A/Bs the same
function against itself — same top-k tail, only the loop body differs. N=100k,
75 seeds, 25 iterations, min of 7, the quietest of three runs (a loaded box gives
this a ±20% floor, which is wide enough to hide the whole effect on any single
row — the shape across rows is the finding, not any one of them):

| reached | out-degree 4 | out-degree 8 | out-degree 16 |
| --- | --- | --- | --- |
| ~58–60% | 0.90 | 0.82 | 0.88 |
| ~78–80% | 1.08 | 1.03 | 0.92 |
| ~88–90% | 1.13 | 1.06 | 1.06 |
| ~93–95% | 1.17 | 1.17 | 1.11 |
| ~97–100% | 1.22 | 1.16 | 1.29 |

Each cell is confined-only ÷ full-width-once-closed: above 1.00 the confined walk
is the slower of the two.

**The crossover sits at ~80% reach, and reach alone does not decide it.** Below
it the confined walk wins by 0.82–0.92×; from 88% up the full-width loops win by
1.06–1.29×, monotonically in reach, at out-degree 4, 8 and 16 alike. Out-degree
2 never gets past 76% reach on this generator and never crosses; out-degree 1
does not close inside 25 iterations at all, so no switch can fire there and none
needs to.

What ships is `closed && reached * 100 >= n * 90`. 90 rather than 80 because the
two errors are not symmetric — switching too late gives back the 1.1–1.3× band,
switching too early costs up to 1.22× on graphs the confined walk still owns. It
is exact for the same reason the confinement is: every node outside the reached
set holds a literal zero in both vectors, and `x + 0.0 == x` for every
non-negative finite `x`, which is all rank and teleport mass ever are.
`confined_iteration_equals_the_full_width_one_bit_for_bit` now runs its whole
matrix three times — never switch, switch at 90, switch the moment the set closes
— against the one full-width reference, over two graphs placed at 88% and 92%
reach so the threshold is crossed inside the comparison, and it fails if the
matrix walked only one of the two bodies.

**The allocation closed 2026-07-22, and the sparse rank vector was never needed.**
The item said removing the buffers means a sparse representation, and that a hash
map's iteration order would put the `+0.0` argument back in play. That reasoning
had the wrong target. What the buffers cost is the *allocation*, not the *width* —
so the width can stay, dense and ascending and bit-identical, and only the
allocation goes. The four vectors are now lent by the calling thread and handed
back zeroed over the reached set alone, which is the same set the walk already
pays for. No arithmetic moved and no index order moved, so nothing had to be
re-argued: `confined_iteration_equals_the_full_width_one_bit_for_bit` passes
unchanged, and it is a *stronger* test than it was, because its 540 comparisons
now run through one thread's reused buffers — a value left behind by any of them
would surface as a wrong score in the next.

**Measured with an allocator, not a clock** (`floor_by_graph_width_at_fixed_reach`
and `allocation_and_floor_by_reach` in `src/retrieval_pagerank.rs`, both ignored
by default; the counting allocator is `test_support::alloc_probe`). Timing cannot
witness this — 2.5 MB of `calloc` is under the noise of a box with two sibling
worktrees on it — so the gate is the byte count.

| | per-call bytes | largest single block |
| --- | --- | --- |
| before, N=100k @ 1.0% reach | 2,540,344 | 800,000 |
| after, same | 40,344 | 16,384 |

25.40 B/node before, which is exactly the four: 8 + 8 + 8 for `tele`, `rank`,
`next` and 1 for `in_reached`. The largest block before is one N-wide `f64`
vector; after, nothing near it exists. Holding the walk fixed at 976 reached
nodes and moving N alone — 10k, 50k, 200k — the allocation was 300,552 /
1,300,552 / 5,050,552 B before and **50,552 B at all three** after, with the
largest block 80,000 / 400,000 / 1,600,000 B before and 16,384 B throughout.

**What the clock says, which is less than the item claimed.** N=100k at 1.0%
reach, min of 7, four runs a side: **0.310–0.420 ms before, 0.244–0.249 ms
after**. So ~0.065 ms, not the 0.18 ms the item recorded — that 0.18 was the
whole per-query cost at low reach, and the allocation was about a third of it.
At 10% reach it is 5.04–7.66 ms against 4.49–4.64 ms. The direction is
consistent across every paired run; the magnitude is small and should not be
quoted as a headline. The 50 / 90 / 100% rows swing 19–186 ms on this box either
side of the change and decide nothing at all.

**Named tradeoff: the buffers are now resident per thread, not transient per
call.** A thread that has ranked an N=100k graph keeps 2.5 MB until it dies, and
retrievals run under a read lock so concurrent readers each hold a set. Peak RSS
is not worse than before — the old path allocated the same 2.5 MB per concurrent
call — but the steady state is, by thread count. The first call on each thread
still pays the full 2.5 MB, which the bench reports as its `first=` column.

**What this leaves, which is not this item.** The remaining per-call allocation
is proportional to *reach*, not to N: `reached`, `fresh`, `merged` and `scored`
still churn, and at 100% reach a query allocates 4,869,552 B (down from
7,369,552). That is cost proportional to work actually done, which is a different
claim from the one this item made, and it is recorded here rather than opened.
Separately, time at fixed reach still rises with N — 0.19–0.28 / 0.20 / 0.29–0.35
ms at 10k / 50k / 200k after the change, against 0.21–0.30 / 0.25–0.32 /
0.36–0.43 before — so something in the call still tracks graph width. It is not
these buffers; the byte counts above are flat across all three. Not chased.

Item 25's "PageRank ÷ scan" table above is now stale in one direction only: it
is a correct record of what was measured before this change, and the ratios in
it no longer hold. The scan is the larger cost at every eligibility level tested
here, which is the ordering that item said the numbers would decide.

### 27. A GC sweep pays one LMDB commit, not one per victim — closed 2026-07-22 `[lifecycle]`

One item because one sweep paid all of it. The four costs it opened with are
settled — three closed, one withdrawn — and measuring them found a fifth that
actually dominated the sweep, which was none of the four. That fifth closed
2026-07-22 and is the third bullet below.

**Measured 2026-07-21 before touching anything** (`tests/gc_scale.rs`, release,
one sweep per row). `run_gc` returns before eviction once the victim list is
empty, so a sweep over a kern with zero victims *is* the selection scan and
nothing else:

| N entities | victims | whole sweep | selection scan | selection's share |
| --- | --- | --- | --- | --- |
| 10k | 0 | 0.70 ms | 0.70 ms | 100% |
| 10k | 800 | 4 370 ms | 0.70 ms | 0.02% |
| 100k | 0 | 3.82 ms | 3.82 ms | 100% |
| 100k | 800 | 6 045 ms | 3.82 ms | 0.06% |
| 100k | 80 000 | 278 764 ms | 3.82 ms | 0.001% |

- ~~Victim selection is O(entities) per kern per sweep~~ **Withdrawn 2026-07-21 —
  measured, not fixed.** 3.8 ms at 100k entities against a 6 045 ms sweep. It is
  also linear, never superlinear: one predicate per entity, once per GC interval,
  on a background tick. No index was written and none would help — the predicate
  is decayed heat, a function of `now` and each entity's own `heat_updated_at`
  (`src/tick_stigmergy.rs:49`), so no ordering of entities survives the clock
  advancing.

- ~~The cold tier is a brute-force cosine scan with no index~~ **Closed
  2026-07-21.** It was never the scan that cost: 87–99% of `cold_search` was
  bincode-decoding a whole `Entity` per row to reach its vector. Vectors moved to
  their own LMDB table, so the scan scores off raw floats (`src/base_store.rs:692`)
  and decodes only the k winners (`:709-711`). At the 50k cap, 470 ms → 28 ms per
  call. No index was added: this is on the *recall* path (`src/mcp_tools_query.rs:214`,
  on hot-tier underfill), and an ANN over the cold tier would put a resident index
  back on the tier that exists to not be resident.

- ~~Eviction pays one LMDB commit per victim~~ **Closed 2026-07-22.** It was the
  commit, and only the commit. `cold_spill` and `cold_put_all` encode the same
  rows and issue the same two `put`s per row; they differ only in where
  `write_txn`/`commit` sit, so an A/B over identical batches isolates the
  transaction boundary and nothing else
  (`cold_spill_per_victim_vs_batched`, `tests/gc_scale.rs`, release, dense
  vectors, tier under its cap so no trim pass fires in either column):

  | victims | one commit each | one commit total | per row |
  | --- | --- | --- | --- |
  | 100 | 918 ms | 20.5 ms | 9.18 ms → 0.21 ms |
  | 800 | 7 728 ms | 70.2 ms | 9.66 ms → 0.09 ms |
  | 5 000 | 38 053 ms | 284 ms | 7.61 ms → 0.06 ms |
  | 20 000 | 136 012 ms | 1 060 ms | 6.80 ms → 0.05 ms |

  `run_gc` now hands the whole victim list to `cold_put_all` in one transaction
  (`evict_batched`, `src/tick_stigmergy.rs:136`). Whole-sweep effect, both
  columns measured in one sitting on one machine (so these absolutes are ~2.2x
  the 2026-07-21 table's, which is a different host):

  | N | victims | before | after |
  | --- | --- | --- | --- |
  | 10k | 80 | 496 ms | 9.4 ms |
  | 10k | 800 | 4 660 ms | 28.1 ms |
  | 10k | 8 000 | 67 811 ms | 250 ms |
  | 100k | 800 | 4 367 ms | 34.9 ms |
  | 100k | 8 000 | 46 884 ms | 222 ms |
  | 100k | 80 000 | 615 919 ms | 2 890 ms |

  The ratio this item opened with has inverted: selection was 0.06% of a
  100k/800 sweep and is now 8.3% of it.

  **What a batched failure means was the real question, and the answer is that
  it means nothing new.** A failed batch falls back to the per-victim loop it
  replaced (`evict_victims`, `src/tick_stigmergy.rs:155`), so the retention
  semantics are unchanged: the row that cannot be spilled stays hot and is
  retried next sweep, every other victim is still collected (`kept`,
  `src/tick_stigmergy.rs:177`). All-or-nothing was the alternative and it was
  rejected: cold GC is the only bound on hot-graph size, so one permanently
  un-encodable row would wedge that bound every hour, forever — a liveness
  failure traded for nothing, since a batch that fails has written nothing and
  loses no data either way. The fallback also absorbs a batch too large for one
  LMDB transaction (`MDB_TXN_FULL`) by finishing the sweep slowly instead of not
  at all.

  Two costs accepted. The batch is a clone of every victim entity before the
  commit — ~80 MB transient at the 80 000-victim row above, of data already
  resident and about to be freed. And one LMDB write transaction is now held for
  the length of a sweep rather than V short ones; at 80 000 victims that is a
  single ~2.9 s hold against 616 s of intermittent holds, so any contending
  writer waits strictly less in total, but a single flush can now block ~2.9 s
  instead of ~7 ms. No deadlock is introduced: nothing inside the transaction
  takes the graph lock, and `run_gc` holds that lock exclusively for the whole
  sweep, so no flusher can hold a graph read lock while waiting on the writer.
  Deciding behavior: name-the-tradeoff.

- ~~`cold_cap` decodes the entire 50k-row cold table on every individual spill~~
  **Closed 2026-07-21.** `cold_spill` now calls `cold_cap_amortized`, which trims
  only once the tier is a slack margin (1024 rows) past the cap and then cuts back
  to it — one full-table pass per 1024 spills instead of one per spill. The tier
  may sit up to 2% over its cap between passes; the cap is a disk bound, not a
  correctness boundary. Direct `cold_cap` callers still get the exact trim.
- ~~`HnswIndex::delete` is O(nodes × edges), once per victim~~ **Closed
  2026-07-21.** Deletion marks the node dead (searches skip a `None` node, so it
  is immediately invisible) and queues the slot; one scrub pass clears every slot
  deleted since the last one, and a slot only enters the free list after it. A
  sweep pays one pass instead of V. Symmetry could not be used to do better:
  insert links both ways, but pruning an over-cap neighbour drops its back-edge
  while the forward edge remains, so a node's own layers are not a complete list
  of who points at it.

The previous version of this file listed "HNSW tombstone compaction — dead nodes
accumulate" here. **That was wrong.** There are no tombstones: `delete` scrubs
inbound edges, sets the node to `None`, and pushes the slot onto a `free` list
for reuse (`src/hnsw.rs:136-149`, scrub `:153`, alloc reuse `:109-125`),
guarded by the test "deleted slots were recycled, arena did not grow" (`:755`). The cost is the
scan, not the accumulation.

### 28. GNN training is off the tick, linear in edges, and `gnn_train_refused` reaches `kern health` `[lifecycle]` — CLOSED 2026-07-22

~~`TaskKind::GnnPropagate => do_gnn_propagate(...)` runs inline in `process_task`
on the single tick loop, stalling large kerns.~~ **(closed 2026-07-21.)** The
stall was measured before it was changed, by a new instrument in the shape of
`tests/gc_scale.rs` — `tests/gnn_scale.rs`, `#[ignore]`d, release-only. At the
shipped defaults — `min_thoughts` 128 and `train_epochs` 24
(`src/gnn_propagate.rs:18-19`) — over 384-dim vectors, one propagation cost
0.64s at 128 entities, 6.4s
at 1024, 21.6s at 2048 and 79.7s at 4096. Every other tick task at the same
sizes was sub-millisecond: `stigmergy_gc` 0.151ms, `commit_access` 0.002ms,
`idle_sweep` 0.000ms at N=4096. On the real loop (`tick::start`) the recall
path's own heat write-back — `CommitAccess`, enqueued at
`src/mcp_tools_query.rs:196` — landed in 2.2ms with nothing ahead of it and in
**56,787ms** with one propagation ahead of it at N=2048. The premise held.

Training now runs on a dedicated thread (`src/tick_trainer.rs`), and the tick
arm only hands it the kern id (`src/tick.rs:116-121`). Same measurement after:
1.2ms at N=2048, down from 56,787ms. Decisions, all three deliberate:

- **Where.** One `std::thread`, not `spawn_blocking`: that pool is 512 wide, so
  every kern would train at once and each training allocates a dense
  `num_entities^2` adjacency — 134MB at N=4096 alone.
- **Overlap.** The waiting set is keyed by kern id, so a second request for a
  kern already waiting is *coalesced*, not queued (`Submit::Coalesced`,
  `src/tick_trainer.rs:99`) — the waiting job snapshots the graph when it runs,
  so it already covers what the newer request would have seen. Past
  `TRAIN_QUEUE_CAP` distinct kerns (`:16`) the newest is refused and counted
  (`:104`, `:114`), the shape item 30 settled on, and the count reaches MCP health
  as `gnn_train_refused` (`src/mcp.rs:146`).
- **Panic.** Item 2 is closed, so the inline arm was already contained by
  `run_guarded` — moving the work does not improve that, it *relocates* it, and
  a bare thread would have been strictly worse: the first panicking propagation
  would kill the trainer and every later one would silently never run. The
  trainer therefore catches per job (`src/tick_trainer.rs:78`) and records
  through the same `Queue::record_task_panic` the health surfaces already read,
  so `GnnPropagate` keeps being the one task that reports a contained failure.

Moving training off the loop also opened a write-back race the inline version
could not have: an entity superseded *during* training would have been
re-inserted into `gnn_entity_idx` by the apply step, undoing the supersede
removal. `apply_gnn_updates` now re-checks status at write time
(`src/tick_gnn_propagate.rs:225`).

~~Remaining: **the cost itself is untouched.** A propagation still takes 79.7s at
N=4096.~~ **(closed 2026-07-22.)** The adjacency is now stored as its nonzeros
(`normalized_adjacency_sparse`, `src/gnn_graph.rs:178`; `SparseMatrix`,
`src/gnn_sparse.rs:14`) and `try_forward_graph` multiplies that
(`src/gnn_gcn.rs:45`). One propagation, same instrument as before
(`gnn_train_scale`), both arms run back to back in one session so they saw the
same machine: **5.4s → 3.9s at N=1024, 20.5s → 7.4s at 2048, 73.4s → 11.6s at
4096** — 6.3x at the size the item was written around. The resident adjacency
falls from 134 MB to 0.33 MB.

Those were taken under load average ~9 on 8 cores, and the ratio is
load-dependent in a way worth writing down, because the two arms no longer have
the same bottleneck. Dense is bandwidth-bound on a 134 MB matrix and barely
notices contention — its 73.4s here against 79.7s recorded on a quieter machine.
Sparse is CPU-bound on the linear layers and does notice: the same after-numbers
on an idle machine are **1.6s / 3.3s / 6.6s**, i.e. 12.1x at N=4096. The 6.3x is
the honest floor; 12.1x is what an unloaded daemon gets.

The item's own diagnosis was directionally right and specifically wrong, which
is why it was measured first (`gnn_cost_breakdown`, `tests/gnn_scale.rs`). It
blamed *materialising* the dense matrix. Materialising was the **smallest** of
the three dense costs at N=4096: 11.1% of the propagation, against 65.5% for the
multiply and 12.8% for the per-backward `transpose` the item never mentions.
Together 89.4%, so the remedy was right; had anyone optimised only the named
term, 79.7s would have become 71s.

**Bit-identical, and that is the recall gate.** The aggregation is the only
computation that changed, and `sparse_and_dense_products_are_bit_identical`
(`src/gnn_sparse.rs`) asserts over `to_bits()`, not a tolerance, that the sparse
and dense products agree exactly — in both orientations, at widths 1/5/17/384,
on a degree-2 ring, a complete graph and a graph with a zero-degree sink. Two
properties carry it: the entry is the same `1.0 / (sqrt(di) * sqrt(dj))`
expression over a degree the dense form reaches by summing that many exact
`1.0`s, and columns ascend inside a row, so the sparse product visits the same
nonzeros in the same order the dense one does. The terms it skips are exactly
the stored zeros, and `x + 0.0 * b == x` for a `+0.0`-seeded accumulator and
finite `b` — item 26's argument, same shape. Identical inputs through identical
downstream code give identical outputs, so `gnn_vector` is unchanged to the last
bit and ranking cannot move.

The e2e recall harness is green and unchanged (0.9306 / 0.9722 / 0.9471 before
and after) but is **not** evidence here: `test_recall.py` drives the CLI, its
corpus is 36 facts and `min_thoughts` is 128, so no propagation runs in it at
all. *Restated 2026-07-22:* the sentence here used to end "the recall gate the
previous author asked for does not exist at that corpus size", and item 97 built
it — `tests/e2e/test_gnn_recall.py` lowers `min_thoughts` to 4, waits for the daemon's
own `learned propagation applied` line, and scores recall only after one
arrives. So a GNN recall gate now exists and would have covered this change; it
still measures a 36-node graph, so it is a wiring gate, not a scale gate.

~~Also unaddressed, and now the whole of this item: `gnn_train_refused` reaches
MCP health only (`src/mcp.rs:146`) — the RPC `HealthRes` does not carry the
field, so no CLI can see it.~~ **(closed 2026-07-22.)** Corrected before it was
fixed: this said `kern status`, and that is the wrong command.
`src/commands_status.rs:1-6` says what it is for in its own first line — it
describes the *processes* around the graph, `kern health` describes the graph —
so the counter belonged to `cmd_health` (`src/commands_admin.rs:38`). Both
lacked it, so the defect was real either way; only the target moved.

Three edits and no new plumbing. `HealthRes` took a `#[serde(default)]
gnn_train_refused: u64` (`src/transport/src/kern_rpc/dto.rs:74`) — append-only,
the shape every other counter took, and `dto_serde_tests` feeds it a literal
old-daemon payload to prove a new client against an old daemon reads 0 rather
than erroring. `KernRpcHandler::health` (`src/rpc/kern_rpc_server.rs:113`) fills
it from `u64_at("gnn_train_refused")`, reading the *same* `tool_health` JSON the
MCP surface emits so the two cannot drift. And `tick_health_lines`
(`src/commands_admin.rs:175`) folds it into the existing `degraded:` line.

**Why it folds in rather than joining the fail-open line above it.** `cmd_health`
already prints a `degraded:` line for the seven fail-open counters
(`src/commands_admin.rs:159`), and that is where an eighth looks like it belongs.
It cannot go there. That line is built from `graph_health_stats`
(`src/health.rs:43`), which the CLI computes *in its own process*, while
`TRAIN_REFUSED` is a global only the daemon ever moves — a CLI reading it
locally sees 0 forever. The only counters a CLI can see are the ones that
crossed the RPC inside `HealthRes`, and the tick line is the only
`HealthRes`-derived degradation line there is. Folding into it also keeps
`a_clean_daemon_prints_no_last_fault_lines` true unedited: a healthy tick still
prints exactly two lines, so a quiet kern does not grow a third that always
reads zero.

Two mutations were run rather than argued. Hardcoding the handler field to `0`
fails exactly `a_refused_gnn_training_reaches_the_rpc_health_surface` and
nothing else — that test spawns a real `Trainer` whose runner blocks on a
channel, submits distinct kern ids until `Submit::Refused`, and asserts the RPC
surface reports the count the trainer actually holds. Gating the new segment on
`task_panics > 0` fails exactly
`a_refused_gnn_training_shows_with_no_other_counter_moving` and — this is the
point — leaves `a_clean_daemon_prints_no_last_fault_lines` green, because an
all-zero `HealthRes` cannot tell a printed zero from a suppressed one.

**The real `Trainer` cost something, recorded because it nearly shipped red.**
`TRAIN_REFUSED` is one global per process, and CI runs `cargo test --workspace`
(`.github/workflows/ci.yml:95`) — one process for the whole suite — where `just
test` runs `cargo nextest`, one process per test. The new test refuses a full
cap's worth of submissions, and the trainer's own
`a_backlog_past_the_cap_is_refused_and_counted_not_grown` asserts its delta is
exactly 1, so the two raced. Measured: **5 red runs in 30 under `cargo test`, 0
in 40 under nextest** — which is precisely why `just test` never saw it, and why
a green `just test` is not evidence about a global. Both tests now hold
`REFUSAL_COUNTER` (`src/tick_trainer.rs:43`) across the window in which they
measure; 40 of 40 green after. The rule this leaves: a test that *moves* a
process-global must serialise against every test that *measures* one, because a
measurement is two reads and the gap between them belongs to whoever else is
running.

Two consequences left deliberately unbuilt. The dense `normalized_adjacency`
(`src/gnn_graph.rs:134`) is no longer on any production path; it is kept as the
reference the equivalence test compares against, because a reference that lives
in the test can drift from the thing that shipped and this one cannot. And the
trainer is one `std::thread` rather than a pool specifically because "each
training allocates a dense `num_entities^2` adjacency — 134MB at N=4096" — that
reason is now false, so the concurrency choice is re-openable on its own merits.

### 29. Spilling all three indexes was measured and refused; it costs 122 MB more `[retrieval]`

~~DiskANN spill is entity-index-only, so the memory ceiling is pushed back, not
removed.~~ **The premise is true and the remedy is refused, measured 2026-07-21.**
The two indexes are exactly where the item said: `rebuild_index` hardcodes
`gnn_entity_idx` and `reason_idx` to `VectorBackend::resident(...)`
(`src/graph.rs:289-290`) while only `entity_idx` takes the spill branch
(`:296-297`). What was never checked is whether spilling them would help. It
would not; it costs 122 MB.

The instrument is `tests/spill_memory.rs` — one process per configuration
(glibc does not return HNSW's many ~1.5 KB vector allocations, so a
free-direction reading inside one process is a lie), RSS from
`/proc/self/statm`, and **two readings: cold, and hot after 200 searches.** The
hot one is the honest one, because mmap pages are only resident once touched.
50k entities at dim 384, each with `vector` AND `gnn_vector`
(`src/base_types.rs:287-288`), plus 25k reasons with vectors (`:433`):

| configuration | cold MB | **hot MB** |
| --- | --- | --- |
| kern map alone, no indexes | 260.7 | 260.7 |
| + entity index (resident) | 358.1 | 358.1 |
| + GNN index (resident) | 358.5 | 358.5 |
| + reason index (resident) | 309.6 | 309.6 |
| all three resident — today's default | 510.3 | 510.3 |
| entity spilled — today's `disk_threshold` path | 438.9 | **512.1** |
| all three spilled — what this item asked for | 449.2 | **632.3** |

Three findings, in the order they overturn the item:

1. **Spilling frees nothing under load.** It looks like 71.4 MB (510.3 → 438.9)
   until a query touches the snapshot; then it is 512.1, *above* never spilling.
   The vectors mmap is faulted straight back in, and `DiskIndex` additionally
   keeps `ids: Vec<String>` resident (`src/diskann.rs:304`). What spill
   actually changes is the *kind* of memory: ~97 MB of unreclaimable heap becomes
   clean, file-backed, reclaimable page cache. That is worth having under memory
   pressure. It is not a ceiling moving, and `decisions/diskann-spill.mdx:48`
   claiming heap drops "by the full vector set" was corrected in the same change.
2. **Doing what this item asked makes it worse by 122 MB** (632.3 vs 510.3 hot).
   Three `ids` vectors instead of one, three adjacency mmaps walked at open, three
   builds' arenas retained. Priced by the `spilled_all` mode without shipping it.
3. **The largest resident holder is not an index and never was.** The kern map is
   260.7 MB of the 512.1 — 51% — because every vector is stored *twice*: once in
   `Kern::entities`/`reasons`, once in the index that points at it
   (`HnswNode::vec`, `src/hnsw.rs:14`). Spill relocates one of the two
   copies. Nothing removes either. Halving that needs shared ownership of the
   vector between the kern map and the index, which is a type change across
   ~20 write sites, not an index-backend swap.

**Closed as written.** No index-spilling code shipped. What the pass did ship is
the defect it found on the way: `build_and_save` was **not reproducible** despite
a seeded RNG and a comment claiming it was — two hashed containers reached the
adjacency, and the `sort_by` that ranks candidates is stable, so every tied
cosine distance broke in per-process hash order. Same corpus, different index,
every process. Both now `collect` into a `BTreeSet` (`src/diskann.rs:123`,
`:180`); guarded by
`the_same_corpus_builds_a_byte_identical_index` (22740/76800 adjacency bytes
differ when the first is reverted, 446/76800 when the second is) and by
`tests/spill_transparency.rs`, which also records what spilling costs in recall:
resident 1.0000 vs spilled 0.9940 against brute force, overlap 0.9940. Spill is
therefore **not** answer-preserving — it swaps one approximation for another, and
identical answers were never on offer.

Still open, and belonging to item 83 rather than here: nothing bounds the
resident set, `disk_threshold` defaults to `KERN_CAP_DISABLED`
(`src/config_graph.rs:20`) with no auto-tuning and no signal on approach, and the
double-storage in finding 3 is the actual O(N) term.

### 95. Every ingest entrance now clamps — closed 2026-07-22 `[ingest]`

~~**A raw 1.0 from the watcher.**~~ **Done 2026-07-22.** Confirmed before fixing:
the sink submitted `1.0`, reaching `beta_params_from_confidence` unclamped and
landing on Beta(2,1) = 0.6667 — exactly a human CLI claim's posterior, and above
the 0.6500 (Beta(1.95,1.05)) a deliberate MCP agent assertion gets. A file
appearing on disk outranked an agent that asserted something on purpose.

**The bypass was wider than the title.** `intake.rs`'s `drain_document` minted a
raw `1.0` for a `Source::File` `Document` too, through `run` rather than
`submit`. Clamping inside `Worker::submit` alone — the shape this item proposed —
would have closed one of two live holes and left the other, which is the same
"a convention each caller remembers" failure one method further down.

So the guard went one level lower: `Worker`'s private `job()`
(`src/ingest_worker.rs:39`) is now the ONLY place a `Job` is built, and it
clamps. Every entrance (`enqueue`, `submit`, `run`) takes a
`source_tag` it cannot omit, so a future producer is asked "who is asserting
this?" by the compiler rather than by a convention. The clamp takes the
confidence only; `kind` stays the producer's, or a watched file would be
reclassified from `Document` to `Claim`.

**The `tag` is the channel, `source.scheme()`** — `"file"` for the watcher
(`src/ingest_file_watcher.rs:95`) and for the intake drain. Not `USER_SOURCE`:
no human asserted it. Not `AGENT_SOURCE`: an agent's ceiling belongs to a
deliberate assertion by a non-human principal, and a file changing on disk is
not an assertion at all. No new `"watcher"` constant either — `clamp_confidence`
only separates `USER_SOURCE` from everything else, so a new tag would be a
second name for the same 0.95 ceiling, exactly the label-that-weights-nothing
item 20 refused. `scheme()` is already what `RetrievalConfig::source_trust`
keys on, so `source_trust = { file = ... }` is the lever that actually separates
watcher from agent. The two paths that DO know their channel name it
explicitly: the CLI passes `USER_SOURCE`, MCP and the direct-intake replay pass
`AGENT_SOURCE`.

**Ranking moved for `Document`s, and not at all for the recall corpus.** A
watcher or intake `Document` drops 0.6667 → 0.6500; `e2e` recall is unchanged at
0.9306 / 0.9722 / 0.9471, bit-identical including the worst-probe list, because
that corpus is ingested through `kern ingest` — the one path that still mints
1.0. The recorded baseline therefore stands as measured; nothing to update.

Deciding behavior: fix-the-root.

### 30. The ingest path is bounded, durable, and observable — backstop, LLM channel, and queue depth all landed — closed 2026-07-22 `[ingest]`

~~`Worker::enqueue` fires `tokio::spawn(async move { tx.send(job).await })` and
returns immediately. The channel bound is 64; the spawn set is unbounded.~~
**(closed 2026-07-21.)** What it actually did was measured before it was
changed: not a silent drop but unbounded growth. `tokio`'s `send` only errors on
a closed channel, so every detached task *parked* on a full queue holding its
whole text — 500 offered to a stalled worker, 500 accepted, nothing refused, and
that is the failure the new test reproduces when the bound is removed. Now
`QUEUE_CAP` is the whole bound (`src/ingest_worker.rs:73`): `try_send` refuses
the newest job rather than detaching (`:125`), the refusal is counted
(`:130`) and reaches every health surface as `ingest_queue_refused`
(`src/health.rs:85`, `src/mcp.rs:145`, `src/commands_admin.rs:166`). The
one producer that must not be refused waits instead — `submit` awaits capacity
(`src/ingest_worker.rs:149`) and the file-watcher sink still calls it
(`src/ingest_file_watcher.rs:128`) — now as the fail-open fallback behind item
30's durable backstop rather than as the only path it has; the MCP RAM-queue
fallback gets a `tool_error` (`src/mcp_tools_mutate.rs:327`). Still distinct
from the *tick* queue, which is bounded at 512 (`FEATURES.md:437-438`).

**Corrected 2026-07-22 — the "no timeout budget" half was never true, and the
grep that established it looked in the wrong files.** The claim was "no
`timeout` in `src/ingest_distill.rs` or `src/ingest_worker.rs`", and there is
none in either — because the bound lives at the client, not at the caller.
`distill` takes an opaque `llm: &dyn Fn(&str) -> String`
(`src/ingest_distill.rs:37`) which is always `Client::complete_func`
(`src/llm.rs:388`), and `complete` posted under `LLM_TIMEOUT` = 600s
(now the default behind `[reason] timeout_secs`, `src/llm.rs:492`, applied at
`:344` and `:371` — both the native and the OpenAI-compat branch), over a
client-wide 120s default and a 3s
`connect_timeout` (`:159`, `:162`). So a hung LLM does **not** hold the one
in-flight slot forever; it holds it for at most ten minutes. What is actually
open is that 600s is a ceiling nobody chose for *this* leg: it is a `const`, not
config. On the *bound* the item was narrower than stated, and a tuning question.

**Widened 2026-07-22 — "indistinguishable at every health surface" understated
it. The erasure is one frame below any surface, so no surface could have shown
it.** `complete_func` (`src/llm.rs:388`) ended
`.and_then(Result::ok).unwrap_or_default()`, so every outcome of `complete`
collapsed to `""`: the 600s timeout, a refused connection, an HTTP 500, an auth
rejection and `EmptyCompletion` were one value, with no log line and no counter
anywhere. `is_transient` (`src/llm.rs:21`) exists and classifies
exactly those cases, but it is consulted only on the *embed* leg
(`src/llm.rs:228`) — the completion leg never sees the error it would sort.
Six call sites take that closure and none can tell the cases apart:
`spawn_intake`'s two (`src/commands.rs:681`, `src/commands.rs:693`), the intake
tool's kinds closure (`src/mcp_tools_intake.rs:22`), the standalone tick
(`src/commands_mcp_cmd.rs:483`) and `kern profile`
(`src/commands_profile_cmd.rs:93`).

The downstream handling is correct, and that is precisely what hides it:
`distill` reads `""` as `None` (`src/ingest_distill.rs:34`, contract at
`src/ingest_distill.rs:29-33`), so the delta is retried and never archived —
nothing is lost. But a hung endpoint, a dead endpoint and a model too weak to
emit a JSON array all produce the same unbounded retry loop. **The stuck sidecar
concedes it in so many words:** `record_stuck` (`src/ingest_intake.rs:167`)
writes "the reason model returned no parseable claims (prose reply, or endpoint
unreachable)" — the product's own error text names two causes and admits it
cannot say which, because by then there is nothing left to say it with. So the
residue is two things, not one:
**choose the ceiling** (config, not `const`) and **give the failure a channel** —
a counted, logged error on the completion leg, the way `ingest_queue_refused`
counts the queue's. The second is the liveness half the item denied it had.

**The durability half is closed 2026-07-22.** `KernFileWatcherSink::ingest` now
writes a `DirectJob` through `intake_direct` first
(`src/ingest_file_watcher.rs:104`, tmp + rename at `src/ingest_direct.rs:44`) and
falls through to `Worker::submit` (`src/ingest_file_watcher.rs:128`) only when
that write fails — the shape `tool_ingest` already had
(`src/mcp_tools_mutate.rs:300`). It is gated on `intake.enabled` alone
(`src/commands.rs:1008`), which is exactly the flag
`spawn_intake` gates on, so the directory is never written unless something
drains it; `drain_direct_once` needs no reason LLM, so the stricter gate
`tool_ingest` uses would have parked nothing on a reason-less host that drains
fine. `notify` installs its watches and reports nothing that happened before
(`FileWatcher::new`, `src/watcher.rs:36`) and there is no startup
scan, so before this a watcher record still in the channel when the daemon died
was lost with nothing to re-offer it.

`DirectJob` grew a `source_tag` (`src/ingest_direct.rs:30`) to make that hop
safe: `drain_direct_once` used to name `AGENT_SOURCE` for every payload it read
(now `:101`), because every payload there was minted by the MCP tool — routing
the watcher through it unchanged would have relabelled `"file"` as `"agent"` and
undone item 95's "the tag is the channel". Old payloads keep the old behaviour
through `#[serde(default)]` (`:38`). The relabel is numerically invisible —
`clamp_confidence` separates `USER_SOURCE` and nothing else — but it is the key
`source_trust` weights on (item 20).

**The fix had a self-referential edge, found and closed in the same change.**
The default watched root is the cwd and the default intake is `.kern/intake`
under it, so parking a record durably wrote a file into the tree that produced
it: the watcher read it back, parked a payload wrapping that payload, and
repeated. Measured against the default config from one seed edit: **283 payloads
in 60 seconds, largest 1.77 MB, versus 0 on the pre-change build** — each one an
embed call and a graph write. `IgnoreRules` only ever hardcoded `.git`
(`src/watcher_ignore_rules.rs:60`), so nothing stopped it. Closed by giving
`IgnoreRules` host-supplied denied prefixes (`:45`, matched `:63`) and passing
the resolved `intake.dir` and `data_dir` (`src/commands.rs:999`) — named by the
host, because that crate must not know what kern is. `effective_roots` now pins a
relative root to `cwd` (`src/config_watcher.rs:25`) so event paths and denied
prefixes share one coordinate system. What is *not* closed is that the deny list
is by name rather than by construction — item 99.

The queue-depth half is
**narrowed 2026-07-21** — closing item 8 gave `kern intake` a
`pending=/stuck=/failed=/done=` readout (`src/commands_intake_cmd.rs:43-45`),
so the file-backed queue reports its depth; the in-process `Worker` channel
still does not. "The only LLM call on the path"
(`concepts/acceptance.mdx:7` — the old citation, to lines 189-192, was past the
end of an 86-line page and quoted a sentence that is nowhere in the tree; it is
spelled without backticks here because a backticked span *is* an anchor to
`docs_check`, and a mention of a dead citation is not one), and with the
answer leg removed (2026-07-21) the distill leg is still the only LLM on any
path — no latency work has landed on it.

**Both halves of the LLM residue are closed 2026-07-22.** *The ceiling is
chosen:* `LLM_TIMEOUT` is now the default behind `[reason] timeout_secs`
(`src/config_reason.rs:12`, `DEFAULT_REASON_TIMEOUT_SECS` = 600 at `:20`),
threaded through `Client::with_timeout_secs` (`src/llm.rs:196`) at every site
that builds a client from config (`src/commands.rs:498`,
`src/commands_ingest_cmd.rs:57`, `src/commands_intake_cmd.rs:143`,
`src/commands_graph_ops.rs:340`, `src/commands_profile_cmd.rs:51`). *The failure
has a channel:* `complete_func` (`src/llm.rs:388`) still hands its caller `""` —
that contract is what six call sites rely on — but the error is counted and
named on the way past (`record_complete_failure`, `:74`), reusing `is_transient`
(`:21`) rather than adding a second classifier, and reaching the surface as
`llm_complete_failed` / `last_llm_complete_failure` (`src/mcp.rs:153`,
`trnsprt::HealthRes` at `src/transport/src/kern_rpc/dto.rs:80`, printed by
`llm_health_lines`, `src/commands_admin.rs:214`). Daemon-sourced, because the
counter is a process static and nothing on the `kern health` path completes
anything — a local read could only ever be zero.

**What the count buys is exactly `record_stuck`'s missing sentence.** The four
outcomes it could not separate now read apart on one health line: a hang as
`transient: HTTP error: error sending request for url (...)`, a refusal as the
same class with a different message, a 5xx as `transient: API error (500): ...`
(first line only, capped at 160 chars — a gateway's error page is HTML and would
otherwise take the screen), an empty body as `permanent: empty completion
response`. And prose from a weak model raises **nothing**: the endpoint is fine,
so the counter that means "the endpoint is at fault" stays at zero, which is the
one bit `record_stuck` needed and did not have. Proved four ways rather than
asserted: `llm.rs`'s four-mode test compares the recorded strings pairwise for
distinctness and takes a *delta* off the process-global counter
(`src/llm.rs:764`) — the shape `src/ingest_worker.rs:459` established, which
reads `before = ingest_queue_refused()` and asserts the difference, because
`cargo test` runs the whole crate in one process and an absolute count is green
under `nextest` and red under it; a control test proves a working model moves
nothing; and
`tests/e2e/test_llm_failure_channel.py` drives hang / 500 / empty-body through a live
daemon's intake poll and reads the line back over the socket. **Unchanged when
unconfigured, measured not claimed:** the default `timeout_secs` is asserted
equal to the `const` it replaced and applying it is asserted a no-op
(`src/llm.rs:726`), and `0` reads as unset rather than as a zero deadline
(`src/llm.rs:196`).

**The queue-depth half is closed 2026-07-22, and the item with it.**
`Worker::queue_depth` (`src/ingest_worker.rs:148`) reads the mpsc channel's own
occupancy — `max_capacity - capacity` — so the gauge cannot drift from the queue
it describes: there is no counter to increment and nothing to forget, and a job
the run loop has taken in flight releases its slot and is not counted. It rides
beside `ingest_queue_refused` on all three surfaces: the MCP health JSON reads
it live off the serving worker's channel (`src/mcp.rs:148`) rather than off a
process static, `HealthRes` carries it `#[serde(default)]`
(`src/transport/src/kern_rpc/dto.rs:70`) so an old daemon's payload reads 0
instead of erroring, and `kern health` prints `ingest: queue N` daemon-sourced
only (`ingest_health_lines`, `src/commands_admin.rs:203`) — no daemon, no line,
because the CLI's own worker is idle by construction and a local read is
structurally zero (item 100's rule). The refusal counter says the bound was hit;
the depth says how close it is now. Proved at both ends: the unit test parks
five jobs behind an embed gated on a closed semaphore, reads exactly five, and
watches the gauge fall to zero when the gate opens
(`src/ingest_worker.rs:531`), and `tests/e2e/test_health_surface.py` parks
watcher jobs behind a stalled fake-LLM embed and reads the nonzero line back
over the socket from a live daemon — after asserting an idle daemon reports 0
and no daemon prints no line at all.

### 31. Structural debt in the hot types `[retrieval]`

Both routing bullets are retired; what remains is serialization and index shape.

Recorded in `FEATURES.md` gap blocks, planned nowhere:

- ~~Routing does a vector lookup per level, O(depth·log n), and unnamed children
  are unbounded per parent~~ **(retired 2026-07-21 — verified false on both
  counts; the FEATURES gap block it quoted is corrected at `FEATURES.md:126`).**
  ~~Per-parent fan-out is a real cliff and stays on this item's list~~
  **(measured and retired 2026-07-22 — the width is real and unbounded, but it
  costs a linear ~2% at the widths the graph reaches, and the scan the wording
  blames is not where even that goes).** The location holds:
  `route_to_child_id` (`src/accept.rs:882`) is a linear scan over the
  parent's loaded named children against each child's stored `graviton_vec`,
  reached from `route_entity` (`src/accept.rs:196`) once per accepted
  entity — per distilled claim and per chunk (`src/ingest_place.rs:131`,
  `src/ingest_place.rs:209`) — descending up to `MAX_ACCEPT_DEPTH`, in practice
  two levels. Unnamed children are capped at one per parent on the routing path
  by `get_or_spawn_unnamed_child` (`src/accept.rs:631`, guarded by
  `src/accept.rs:921`); only tick clustering makes more, one per spawnable
  cluster and deliberately (`src/tick.rs:196`).

  Instrument: `tests/route_fanout.rs`, release, `--ignored`. Fan-out costs
  **0.14-0.18us per child** across runs, of an accept that costs **1.4-2.1ms**
  at 20k entities — 0.5% at width 64, ~5% at 512, ~24% at 4096. The accept is
  dominated by the two HNSW searches it runs (the dedup gate at
  `src/accept.rs:39`, the similarity reason at `src/accept.rs:391`)
  plus two index inserts, which is why width has to reach the thousands before
  it registers. A named/unnamed A/B over the same walk — identical descent,
  cosine skipped — attributes **-0.009, -0.001 and +0.003us per child** to the
  `graviton_vec` comparison on three runs: zero every time. What the width actually buys is two
  `Vec<String>` clones per descent (`src/accept.rs:218`,
  `src/accept.rs:683`) and a linear resident-map probe for the generic
  child; the comparison the bullet named is free.

  Nothing caps named children per parent, and unlike the other two claims this
  one survives measurement. Only two things create a child with a routable
  `graviton_vec`: `add_graviton_with_mass` (`src/accept.rs:756`),
  human-declared and root-only, and tick naming (`src/tick_tasks.rs:239`), whose
  result `promote_to_root_if_generic` (`src/accept.rs:821`) lifts to root.
  Driving that real accept → cluster → name → promote loop, root fan-out tracks
  distinct cohesive topics very nearly 1:1 — 8 topics -> 8 children, 64 -> 55,
  256 -> 191. `GRAVITON_DEDUP_THRESHOLD` collapses only topics whose graviton
  names embed within 0.85 of each other, which is a fact about the corpus, not a
  bound on the structure. Disabling promotion does not shrink the width, it
  relocates it: `generic` then holds all 191 and routing scans them one level
  deeper.

  So the width is real and the cost of it is linear — ~2% of an ingest at 191
  children, and it needs some thousands of distinct cohesive topics before it is
  a fifth. That is a slope, not a cliff, and no optimisation is shipped for it.
  Recording the lever in case the slope ever matters: it is the `Vec<String>`
  clone, not an index over children. ~~The clone in `route_entity` exists only to
  end a borrow and can be replaced by holding `&kern.children` alongside the
  `&GraphGnn` the scan already takes; an index would be write-path work on every
  spawn and every rename, buying back a comparison that measures as free.~~
  **The `route_entity` clone lever closed 2026-07-23.** `route_entity`
  (`src/accept.rs`) now holds `&kern.children` alongside the `&GraphGnn`
  reborrow in a scoped block — both immutable, the borrow ends at `}` before
  `current_id = child_id` — so the `Vec<String>` alloc per descent is gone
  (bit-identical routing). Proved by `route_entity_does_not_clone_children_per_descent`
  (children vs no-children delta equal within 8 B; re-add `.clone()` of 4 vs 1
  pushes ~72 B past → reds, green on revert). Routing tests green unedited.
  `cargo test -p kern --lib` 953 passed, 0 failed, 4 ignored. Decided by
  fix-the-root (drop the alloc the borrow needed, not an index), name-the-
  tradeoff (the linear scan stays — item names + rejects an index: write-path
  work on every spawn/rename for a comparison that measures free), verify-
  before-claiming (negative control). See the 2026-07-23 CHANGELOG entry.
- `Entity` is a ~30-field flat struct (serialization cost on every store round
  trip) and `Kern` carries no per-kern stats — mean heat, fill ratio — that
  clustering could reuse (`FEATURES.md:85-87`).
- DiskANN is build-once; the lexical index is RAM-only (`FEATURES.md:245-246`).
- LMDB compaction is manual and offline-only, and is the only way to shrink the
  high-water mark (`FEATURES.md:328-330`).

### 32. Tree depth was an eviction bias in the opposite direction — closed 2026-07-21 `[lifecycle]`

**Closed 2026-07-21. The bias was real; its direction and its severity were
both stated backwards, and the fix the title implies would have made it worse.**

Two corrections to the item as written. The reach is **5 levels, not ~4**:
strength starts at 1.0, halves per level (`PULSE_DECAY`,
`src/base_constants.rs:55`) and the walk returns below 0.05 (`PULSE_THRESHOLD`,
`:56`), so depths 0–4 were all deposited on. And "invisible to any metric that
does not exist yet (item 1)" was stale — item 1 is closed, the harness exists,
and this was measurable the whole time. It is now measured:
`tests/depth_bias.rs` runs the real `pulse` → `commit_access_ids` → `run_gc`
lifecycle over simulated months, two cohorts per depth with identical usage.

The measurement inverted the item. Deep entities were not decaying unfairly —
they were decaying *correctly*. Shallow ones could not be collected at all. The
pulse deposit recurred every 60s, so it never evaporated: equilibrium heat at
depth 4 was 1939 against a `COLD_HEAT_THRESHOLD` of 0.01, and at depth 0 it was
31 031. Anything within 4 levels of the root was permanently exempt from cold
GC, whether or not it had ever been read — which is the vision test "the hot
graph stays bounded" failing, not merely a fairness question. Any recurring
deposit above ~1.6e-7 produces that exemption, so no tuning of the deposit size
could have preserved the pheromone story; propagating *deeper*, which the title
implies, would have extended the exemption to the whole graph.

So the deposit is gone (`src/tick_pulse.rs`), along with
`HeatConfig::deposit_traversal`. Access is the only deposit; the pulse still
fans clustering, GC, reembed and idle-sweep out from the root, so an idle daemon
still maintains itself. Post-fix, every depth 0–7 evicts its unused cohort on
the same day and keeps its used cohort. What this did **not** buy: item 83.
Eviction now fires where it never could, but `max_kerns` and `disk_threshold`
are still `usize::MAX` and there is still no per-kern entity cap, so nothing
bounds the graph *deterministically* — only usage does.

---

# Tier 5 — federation, the rest

All of this is gated behind gossip being on, which is off by default and marked
`building`. Item 33's transport work is the hinge: several items below cannot be
built without it, and the hub's phase 3 ships with it.

Phase 1 landed inline — lamport-stamped LWW on `Reason.score` and `valid_until`
(`src/merge.rs`), a `PendingDelta` queue and a `start_delta_flush` Delta
sender. `crdt.rs` is still 90 LoC of `GCounter` only; the LWW semantics live as
inline fields, not named types. Fine. The OR-Set-for-`statements` plan was
**reversed, not deferred**: `id == content_hash(text)`, so importing remote
statement text both breaks content-addressing and resurrects locally-cleared
statements. Merge never imports them (`src/merge.rs:115`) and the wire
target is rejected on receipt (`src/gossip_handler.rs:502`), kept as a refused
variant so an older peer cannot inject text under a content-addressed id.

### 33. Transport security `[federation]`

Raw TCP, no TLS. `network_id` broadcast cleartext over UDP multicast. No
signature on `GossipMessage` — it carries `kind` / `id` / `origin` / `payload`
only (`src/gossip_types.rs:18-23`) — and `handle_conn` accepts any stream while
`handle_peer_exchange` trusts any `msg.origin`. Needs `tokio-rustls` + `rcgen` as
direct deps; neither is in `Cargo.toml`. **This one gates any deployment off a
trusted LAN / WireGuard mesh**, and it gates the counter-slot identity half of
item 13.

### 34. The `Question` path is an unauthenticated membership oracle `[federation]`

A peer sends `Question` messages carrying arbitrary embedding vectors and gets a
yes/no on whether you hold a fact above cosine 0.80. Content existence is
extractable one probe at a time without ever receiving the content.

**Mitigated 2026-07-21, not closed.** A per-origin budget (`src/gossip_rate.rs`,
30/minute) makes bulk extraction expensive; the oracle is still there for a
patient prober, and `origin` is an unauthenticated self-declared string, so the
budget is evadable by rotating it. Closing this needs an identity to refuse on —
**gated on item 33**, and the same rotation problem is item 35.

### 35. Namespace rotation is unbounded storage `[federation]`

`network_id` / `kern_id` are attacker-chosen, and the quarantine cap is global
per remote kern, not per peer (`concepts/security.mdx:243-246`,
`decisions/knowledge-not-gradients.mdx:113-114`). One host cycling identifiers
creates unlimited `remote-*` kerns, each with its own 50k allowance. Item 39's
Sybil work covers ranking; this is disk.

### 36. Anti-entropy `[federation]`

No `AntiEntropy` variant in `GossipKind` (`src/gossip_types.rs:7-15`). The sender
sorts by heat and truncates to `ENTITY_SYNC_BATCH = 32` per heartbeat
(`src/gossip_handler.rs:156`, sorted `:181`),
so cold entities may never propagate and a partitioned node that rejoins never
catches up. (`Fetch` is live — `wire_fetch` installs the handler at
`src/commands.rs:1099` and the question path issues it — but it is single-id, not a
catch-up mechanism.) Two pieces adopted on paper and unscheduled: **back-off
pacing** with exponential jitter keyed to a divergence estimate
(`docs/fl-vs-knids-federation.md:163-168`), and **batch-size / push-vs-pull
tuning** at scale (`howto/memory-bank.mdx:149-150`) — the top-32 is hard-coded and
the push-only choice was never revisited.

### 37. Backpressure, divergence metric, and delta write-lock starvation `[federation]`

The only per-origin budget is the `Question` one item 34 records
(`src/gossip_handler.rs:318`, 30/min); the `Delta` path — the one that takes the
write lock — has none. `HealthStats` has no divergence field
(`src/health.rs:8-38`). Sharper than previously recorded: the four
full-corpus loops in `handle_crdt_delta` (`src/gossip_handler.rs:432`, `:448`,
`:461`, `:482` — two `g.all_ids()`, two `remote_kern_ids(&g)`, which is
`all_ids()` filtered at `:545`) run under the graph **write** lock, once per
inbound delta, unlimited — a cheap remote write-lock-starvation vector
independent of the local-row mutation in item 13.

~~Beside it: `start_entity_sync` clones the entire local corpus every
heartbeat~~ **Closed 2026-07-21.** `hottest_local` selects over references and
deep-clones only the winners — linear, with the same comparator and therefore the
same chosen set. The rest of this item (per-peer rate limits, a divergence field
on `HealthStats`, and the write-lock starvation from the four full-corpus loops in
`handle_crdt_delta`) is still open. The `Delta` path still has no per-origin
budget; only `Question` does.

(Remote heat is no longer pinnable: entry to a `remote-*` kern strips heat,
access counts and confidence to neutral — `src/merge.rs:20`, applied `:153`.
The pin risk that remains is item 15's unclamped `Pulse`.)

### 38. Peer authority is unbuilt, so Sybil defence has nothing to weight `[federation]`

No DB- or peer-level authority signal exists: "a peer cited by thousands of other
peers was treated identically to a brand-new peer"
(`decisions/pagerank-authority.mdx:96-98`). The full design — an
`AuthorityTable`, TrustRank seeding, `authority_weight` / `authority_floor`
config, a `kern authority seed` admin command — is written out at
`docs/pagerank-authority.md:164, :202-235, :274` and was never
scheduled. It ranks immediately above the Sybil work because the defences in 39
are weightings of a signal that does not exist.

### 39. No Sybil defence is in effect `[federation]`

And, corrected on inspection, none ever was. Two were written and never wired:
`RateClipper` (the since-deleted `gossip/sybil.rs`, 175 LoC) whose `set_clipper()`
had no call site in any commit, and `trimmed_mean_merge_hits`
(`gossip/merge.rs`, 241 LoC), self-described as "a Sybil-resistant alternative"
for fusing per-peer hit lists, also callerless. Both were deleted in `dc02a18` as
verified-unreachable; the deletion changed no behaviour because neither had ever
run. This is **unbuilt work with a reference implementation in git**, not a
regression — materially cheaper than it first appears. The layered defences from
the authority design (edge-weight caps, pulse-coupled edge validation, temporal
slashing of frequently-superseded producers) were never written at all.

### 40. Remote-injected text is retrievable and reaches an agent's context `[federation]`

Remote entities are vector-indexed on insert, so with gossip on, recall output —
and therefore any agent consuming it — extends to every host on the segment
(`concepts/security.mdx:247-251`). Bounded by ranking-signal stripping
(`src/retrieval_score.rs:107-117`), not by exclusion. Decide whether `remote-*`
should be opt-in-per-query rather than indexed by default.

### 41. Two FL-derived bounds adopted on paper, neither in effect `[federation]`

Trimmed-mean / median materialisation for federated scalars (written, never
called, deleted in `dc02a18` — see item 39; recoverable from git), and a
provenance ledger of per-thought `(origin, lamport, confidence)` enabling
retrospective down-weighting of a peer later deemed untrusted, which was never
written — the shipped `Ledger` (`src/gossip_ledger.rs`) is a TTL- and cap-bounded
routing cache (`:24, :28, :63`), enough to know where to fetch, not who told you
what. Adopted-partial and unscheduled beside them: **secure aggregation for
pulses and counters** (`docs/fl-vs-knids-federation.md:131-136`).

### 42. The gossip wire has no version negotiation `[federation]`

The `anchor_*` → `graviton_*` rename "breaks federation peers that predate the
change" (`concepts/stigmergy.mdx:173-175`) and nothing on the wire negotiates a
version. Known impact of planned changes: `GossipMessage.signature` is breaking
(alpha: peers upgrade together, no wire mitigation); `AntiEntropy` is additive. Confidence isolation
(`conf_alpha` / `conf_beta` / `unlinked_count` never imported from remote) must
survive every change — and note that `decisions/crdts-over-consensus.mdx:116-117`
frames the same `unlinked_count` behavior as a **PN-Counter that was never
built**, where this file treats it as a load-bearing invariant. Reconcile: it is
an invariant.

### 43. CRDT growth and re-embedding across replicas `[federation]`

Two leads from `docs/crdts-federation.md`, adopted and never scheduled:

- **Tombstone and LWW-history growth is unbounded** (`:278-281`); the note's own
  follow-up was "time-bounded compaction".
- **Vector LWW is coarse across heterogeneous embedding models** (`:284-286`),
  and `docs/fl-vs-knids-federation.md:200-204` explicitly *allows*
  per-node model choice. Item 3 covers the local swap; the federated case — no
  model-identity stamp on the wire — is separate and unfunded.

### 44. Bi-temporal stamps are never federated `[federation]`

`valid_from` / `valid_to` / `invalidated_at` are `#[serde(skip)]`
(`src/base_types.rs:311-316`), so each node re-derives its own `as_of` view and
two *converged* nodes can answer the same point-in-time query differently
(`docs/crdts-federation.md:54-62`). The federated twin of item 4.

### 45. Multicast discovery is unreliable with no health signal `[federation]`

Wireless APs, container bridges and VPN interfaces all break it, with no
fallback and no way to distinguish discovery-failed from no-peers-present
(`concepts/federation.mdx:68-70`).

### 46. One fresh TCP connection per gossip message `[federation]`

`TcpStream::connect` per call at `src/gossip_transport.rs:37` (`send_msg`) and
`:45` (`send_and_receive`). No pooling. Separately, the `trnsprt` client has no
pooling either (`FEATURES.md:971-972`) — that one is not gossip and is not gated
on 33.

### 47. Hub phase 3: gossip moves hub-side `[hub]`

One UDP endpoint and one node identity per machine; nodes stop binding the
network entirely (`src/config_gossip.rs:7-16` today). **Ordering decided
2026-07-20:** the senders and semantics build per-node first; this transport move
ships together with item 33 — same wire layer, migrate once. Not blocked,
sequenced. One clause of the previous version was wrong: there is **no** per-project
port-clash validation in `src/config_serve.rs` to collapse. (Corrected again
2026-07-21: that file is not `mcp_token` handling only — `ServeConfig` is
`{mcp_token, mcp_addr}` at `:6-11`, and `mcp_addr` is the reader-less field
item 84 owns.)

Beside it: **hub↔node version skew is unmanaged** beyond same-binary spawning
(`FEATURES.md:1077-1078`).

### Decisions owed before the federation build

Deciding behavior: **all 7 decided 2026-07-22 (a–g). TOFU + config-owned network_id close the security pair; the build unblocks on the item 33 transport move.**

- (a) *Subsumed by item 13* — `Reason.score` stays LWW. It was owed as a
      trust-signalling question (max-join would silently revert deliberate
      `degrade_entity_reasons` lowering); the local-row exposure settles it.
      Recorded here rather than deleted so the reasoning survives.
- (b) ~~Anti-entropy watermark shape: vector clock or content-hash bloom?~~
      **Decided 2026-07-22: content-hash bloom.** kern's ids ARE content hashes
      and the CRDT is content-addressed; a vector clock adds a per-replica
      counter the graph has no other use for, while a bloom filter over the
      live content-hash set is the same shape anti-entropy already needs to
      ask "do you have these". Smaller wire, no clock drift, converges on
      content equality (the thing that actually matters).
- (c) ~~TLS cert authority: operator PKI or TOFU pin?~~ **Decided 2026-07-22:
      TOFU pin.** kern's stated values are local-first, zero-config and
      *coordinator-free* (`VISION.md`); operator PKI requires a CA the operator
      runs, which is a coordinator the federation explicitly refuses to need.
      TOFU pins the first-seen peer key and warns on change — zero-config, no
      authority, the same shape SSH first-contact takes. Trade taken: a
      first-contact MITM is not detected (an attacker present at the first
      handshake pins their own key); the mitigation is operator verification of
      the pin out-of-band, the same way SSH's `known_hosts` leaves it. An
      operator who wants managed trust can amend this to PKI; the rest of the
      wire does not depend on which.
- (d) ~~Does `network_id` derive from the cert or stay config-owned?~~ **Decided
      2026-07-22: config-owned.** Under TOFU (c) there is no cert to derive
      from at first contact; `network_id` stays the operator's `[gossip]
      network_id` config key (`GossipConfig::effective_network_id`), and a
      peer whose configured id differs does not federate — the existing
      `effective_network_id` guard, not a new one.
- (e) ~~Does graviton `mass` federate at all, or stay per-node tuning?~~
      **Decided 2026-07-22: per-node, does not federate.** `mass` is local
      routing tuning (how hard a graviton pulls at acceptance); federating it
      lets a peer shift another node's acceptance routing silently. The
      graviton's *existence* (name + seed text + vector) federates as content;
      its mass stays the operator's per-node knob, the way
      `RetrievalConfig::source_trust` is per-node and never federated.
- (f) ~~`superseded_by` conflicts resolve to the lexically greater id … Keep,
      or resolve on lamport then id?~~ **Decided 2026-07-22: lamport-then-id.**
      Lex-greater id guarantees agreement, not the better answer — two replicas
      can deterministically converge on the wrong successor. Lamport-then-id is
      still deterministic (lamport is a total order under the existing
      `lww_wins` tiebreak, id is the documented final tiebreak) and picks the
      *later* claim, which is what a supersede means. `superseded_by_join_picks_
      the_lexicographically_higher_id` already pins the join; the wire change
      ships with the item 33 transport move.
- (g) ~~Do peers running different embedding models federate at all (item 43)?~~
      **Decided 2026-07-22: refuse on embed-model mismatch.** Vectors from two
      embedding models are not comparable — a cosine between a `qwen3-embedding`
      and a `bge` vector is noise, and the HNSW + BM25 + GNN stack assumes one
      space. The store already refuses a mismatched embedder at open
      (`check_embed_stamp`, `src/base_store.rs`); the wire extends the same
      guard: a peer whose `embed_model` differs does not federate entity bodies,
      only the CRDT deltas that carry no vector (the `Reason.score` LWW and the
      G-Counters). No silent cross-model search.

---

# Tier 6 — ingest quality

Below the cliffs because none of it is a defect, above the belief model because
every claim in the graph is shaped here first.

### 48. Source-keyed idempotency at ingest — beside half-closed 2026-07-23 `[ingest]`

**Beside half-closed 2026-07-23 (per-kind dedup threshold, default-off); hard
paraphrase-evadable dedup-key main body still open.** The dedup threshold is no
longer global: `IngestConfig.dedup_threshold_by_kind: [Option<f64>; 5]` (new,
`src/config_ingest.rs`, indexed by `EntityKind as u8`, default `[None; 5]` =
bit-identical today) + `dedup_threshold_for(kind)` resolver (`None` → global
`dedup_threshold`). `validate` rejects out-of-range per-kind `Some` naming the
kind. The three production call sites (`place.rs` `place_document` /
`place_chunks`, `worker.rs`) resolve per-kind; `ingest_cmd` + `mcp tools_mutate`
bridge the array into the runtime `Config`. Array indexed by `EntityKind as u8`
avoids adding `Hash` to `EntityKind` (the `HashMap` alternative would need it).
Proved by `dedup_threshold_for_kind_resolves` (None→global, Some→override,
neighbour isolation), `validate_rejects_out_of_range_per_kind` (1.5 rejected
naming `fact`, `0.0`/`1.0` accepted, NaN rejected),
`per_kind_dedup_threshold_tightens_facts_loosens_claims` (Fact `Some(0.99)`
keeps a `0.97` near-dup the global `0.95` dedups; Claim `Some(0.80)` dedups a
`0.81` the global also catches). Existing dedup + place tests green unedited at
default. `cargo test -p kern --lib` 958 passed, 0 failed, 4 ignored; `cargo test
-p trnsprt --lib` 61 passed. Negative control (Fact slot → `None` → global
`0.95` → `0.97` dedups → Fact "not deduped" assertion reds) green on revert.
Decided by fix-the-root (resolve per-kind at the call sites, not a `HashMap`),
name-the-tradeoff (default-off — no operator asked, no preset sets it; auto-
tuning is YAGNI), verify-before-claiming (negative control). See the
2026-07-23 CHANGELOG entry.

**Still open:** the hard paraphrase-evadable semantic dedup key (main body).

**Measurement half-closed 2026-07-23.** The active ingest dedup config (global
`dedup_threshold` + per-kind `dedup_threshold_by_kind`, shipped above but
invisible at runtime) is now surfaced: `Server::health_stats` (`src/mcp.rs`) JSON
carries an `ingest:` block; `trnsprt::HealthRes` gains
`#[serde(default)] ingest_dedup_threshold: f64` +
`ingest_dedup_threshold_by_kind: [Option<f64>; 5]` (old daemon → `0.0` /
`[None;5]`); `kern health` prints a `dedup:` line daemon-sourced only (item 100
rule), override + all-`None`/global-only + no-daemon shapes; `kern://local/health`
by construction. Proved by `kern_health_prints_dedup_config` + dto round-trip
(`0.95` + `[Some(0.99), None, …]`) + old-payload absence → `0.0`/`[None;5]`
(standing guard). `cargo test -p kern --lib` 965 passed, 0 failed, 4 ignored;
`cargo test -p trnsprt --lib` 61 passed. Decided by fix-the-root (surface the
beside's already-shipped config, do not build the hard key), name-the-tradeoff
(one block — global + per-kind array; tuning is the hard key, avoided),
verify-before-claiming (old-payload-absence guard). See the 2026-07-23 CHANGELOG
entry.

~~`find_duplicate` is pure cosine-over-HNSW (`src/ingest_dedup.rs:8-21`), which is
paraphrase-evadable. **The shape of this item changed and the old wording is
retired:** `CHANGELOG.md` 2026-07-20 shipped chunk external ids keyed on the full
source identity (`source_id()` + chunk index, not the bare section), and CLI
`kern ingest` deriving its inline source hash from the text. What remains is the
*dedup* key, not the external id. Beside it: the dedup threshold is global, not
per-kind (`FEATURES.md:413`).~~

### 49. The distill prompt is one-shot and global — chunking half-closed 2026-07-22 `[ingest]`

**Chunking half-closed 2026-07-22; per-kind branch / label-accuracy half still
open.** `distill` (`src/ingest_distill.rs`) now batches a long conversation into
turn-groups of `DISTILL_CHUNK_TURNS` (new, `src/base_constants.rs`, default
`48`), builds the marked prompt per batch, calls `llm` per batch, `parse_claims`
per batch, and concats the `Claim` vecs — so a long delta stops truncating past
the model's context window with no signal. The common case (`turns.len() <=
DISTILL_CHUNK_TURNS`) stays one call, bit-identical to today. Turn-batched (not
char-batched) preserves the 1-based turn markers `split_turns` produces, so
`Source::Session.section` turn-citations stay well-formed per chunk. The `now`
date injection and `kind_list` are per-batch (each prompt self-contained). A
batch that returns no parseable array (prose / empty) is a format failure for
the **whole delta** → `None` (retry), so a partially-distilled conversation
never archives having silently dropped every later batch. Proved by
`distill_short_conversation_is_one_call`, `distill_chunks_long_conversation_
turn_batched` (N=batch+5 → ceil calls, one claim per batch, none dropped),
`distill_chunk_markers_carry_global_turn_index` (batch 2's first marker is
`[DISTILL_CHUNK_TURNS]` not `[0]`), `distill_batch_format_failure_retries_
whole_delta` (one prose batch → `None` even if an earlier batch parsed).
Existing distill tests green unedited. `cargo test -p kern --lib` 932 passed, 0
failed, 4 ignored. Negative control: `DISTILL_CHUNK_TURNS=usize::MAX` reds the
chunk test (the batch loop collapses to one call), green on revert. Decided by
fix-the-root (chunk the one-shot prompt, do not add a per-kind branch yet),
name-the-tradeoff (turn-batched to preserve citations; cross-chunk context
loses a batch-2 claim referencing a batch-1 turn — chunking > truncating, which
loses everything past the cut; a running summary is YAGNI now), verify-before-
claiming (negative control). See the 2026-07-22 CHANGELOG entry.

**Still open:** per-kind prompt branch / label accuracy — the other half; the
~33% figure is unreproducible (deleted harness), a lead not a number.

~~One `format!` over the whole conversation, no per-kind branch, no chunking
(`src/ingest_distill.rs:34-53`). The `kind` taxonomy has overlapping categories
(decision/project, fact/code-fact) and label accuracy was measured at ~33% even
at 7B — **that figure came from the deleted harness and is unreproducible; treat
it as a lead, not a number** (item 1's claim standard). Long deltas are not
chunked at all.~~

### 50. Intake distillation lacks relative-date resolution — closed 2026-07-22 `[ingest]`

**Closed 2026-07-22.** The distill prompt now injects today's date so a
relative-date phrase ("last Tuesday", "yesterday") resolves to the absolute
ISO8601 `valid_from` `parse_claims` already parses; the capability the deleted
eval path had returned to the product path. `distill` gained a `now:
SystemTime` param (callers pass `SystemTime::now()`, tests pin it) and a new
`date_string` (`src/time.rs`, Howard Hinnant `civil_from_days`, no new
dep) renders it UTC `YYYY-MM-DD` — day resolution, no zone the prompt cannot
name. Direct path unchanged; uncited/undated claims behave as before. Proved by
`distill_prompt_injects_current_date_for_relative_resolution` and `base::time`
round-trips; distill suite 22 green, `cargo test -p kern --lib` 901 passed. See
the 2026-07-22 CHANGELOG entry. Decided by fix-the-root (inject the date, not a
post-hoc parser) and name-the-tradeoff (UTC day, not timezone-aware).

~~The prompt injects no current date (`src/ingest_distill.rs:43-48`), and
`valid_from` is only requested when the statement states an absolute date — so
dropped text containing "last Tuesday" stores unresolved. The eval path got this
and the product path never did; the eval path is now deleted, so the capability
exists nowhere.~~

### 90. `DirectJob` carries `valid_until` but drops `valid_from` `[ingest]` — closed 2026-07-22

**Closed 2026-07-22.** `DirectJob` (`src/ingest_direct.rs`) now carries `valid_from: Option<SystemTime>` with `#[serde(default)]` for backward compat with old payloads on disk. `drain_direct_once` copies both `valid_until` and `valid_from` from the job to `job_cfg`, so the durable intake path preserves the distiller's per-claim lower bound. Callers that don't produce `valid_from` (MCP `tool_ingest`, file watcher) set `None`. Two new tests pin the round-trip and backward-compat deserialization. 912 passed, full workspace green. Decided by: fix-the-root (thread the field, don't re-derive at drain).

### 51. Require reason text on supersede — closed 2026-07-22 `[ingest]`

`ReasonKind::Supersedes` edges are minted at `src/accept.rs:530` and `:625`
with `fallback_label()` text (`src/base_types.rs:116`), never a caller-supplied
rationale. The *why* is the thing the graph exists to hold.

**Closed 2026-07-22.** `commit_reason` now takes a `text` param; `supersede()`
pulls the new entity's statement text and passes it through;
`supersede_by_contradiction()` takes a caller-supplied rationale string.
`fallback_label()` is no longer the sole owner of the text field. 912 tests
pass, full workspace green.

### 52. A single-line graviton seed still truncates at the embed context window `[ingest]`

**Narrowed 2026-07-21 — the old wording is retired.** It said "acknowledged in
source at `src/mcp_tools_admin.rs:116`" with "chunk + mean-pool" as the unbuilt
upgrade path. Both halves moved in `08c9971`: the acknowledgement comment was
deleted, and chunk + mean-pool **shipped** for the multi-line case —
`seed_examples` (`src/accept.rs:717-729`) splits a seed on newlines and
`mean_pool` (`:733`) averages the per-line embeddings, wired at
`src/mcp_tools_admin.rs:119-136`. Line 116 now carries the mean-pool rationale,
i.e. the opposite of what it was cited for.

What is left is the case `seed_examples` deliberately does not split: a seed
whose `lines` len is under 2 is embedded whole (`src/accept.rs:724-726` —
spelled in full because the nearest preceding path is `tools_admin.rs`, which is
455 lines long, and a bare `:NNN` continues the wrong file), so one long
paragraph still goes to the model as a single call and truncates past its
context window with no signal. Chunking *that* wants a length-based split, not a
newline one.

**Mechanism half-closed 2026-07-22 (default-off).** `seed_examples`
(`src/accept.rs`) now char-chunks a single long paragraph at
`GRAVITON_SEED_CHAR_CHUNK` (new, `src/base_constants.rs`, default `4000` ≈ 1000
tokens, well under a 4–8k prompt budget) — when a single-line seed exceeds the
threshold it is split on a code-point boundary into `ceil(len/chunk)` chunks
and returned to the existing caller which embeds each + `mean_pool`s them (no
caller change). Under threshold: `vec![text.trim()]` — bit-identical today (no
real seed is that long). Same default-off shape as item 49 chunking
(`DISTILL_CHUNK_TURNS`) and item 57 (`EVIDENCE_HALF_LIFE_SECS=0`). Proved by
`seed_examples_char_chunks_a_long_single_paragraph` (chunk+5 → 2 chunks, each
`<=` threshold, concat == original; exactly-chunk → 1) and
`seed_examples_char_chunks_split_on_a_code_point_boundary` (multibyte `ß`
straddling the boundary is not split mid-`char`);
`seed_examples_splits_lines_and_keeps_single_text_whole` green unedited.
`cargo test -p kern --lib` 948 passed, 0 failed, 4 ignored. Negative control
(force the single-chunk path → one chunk not two) reds, green on revert.
Decided by fix-the-root (reuse the existing mean-pool caller, return chunks not
a new pipeline), name-the-tradeoff (default-off — the threshold is a knob; a
real truncating-document validation is the operator's, same stance as item 57),
verify-before-claiming (negative control). See the 2026-07-22 CHANGELOG entry.

### 53. Clustering is vector-only `[lifecycle]`

No semantic or structural features (`FEATURES.md:496`), and naming plus
enrich are a cold LLM call per kern. The adopted-but-unbuilt upgrade is
thought-level PageRank feeding the split heuristic — high-rank nodes become
gravitons, bridge nodes become sub-kerns
(`docs/pagerank-authority.md:258-263`, `decisions/pagerank-authority.mdx:120-121`).
Graph structure informs ranking today and never informs the tree shape that
routing depends on.

### 54. GC has no convergence gate `[lifecycle]`

The adopted loop-closing design gated forgetting on convergence — `G ≥ 0.6`
**and** heat below floor for `forget_ttl`
(`docs/stigmergy-self-improving.md:210, :236`). Shipped GC has no gate at all.
Depends on item 62 (the convergence metric) existing.

### 55. Two freshness signals, different half-lives, neither ever tuned `[retrieval]`

A 24-hour one for ranking (`qbst_recency_half_life_secs`,
`src/config_retrieval.rs:31`, defaulted from `QBST_RECENCY_HALF_LIFE`,
`src/base_constants.rs:12`) and the retention one on `HeatConfig`. The offline
NDCG sweep meant to tune either was never run
(`decisions/stigmergy-over-gardening.mdx:117`). Third input nobody reconciled:
`docs/stigmergy-self-improving.md:160-170` derives a 1–2 day half-life.

**Restated 2026-07-21 — the old "7-day retention" wording was stale.** The 7 days
at `src/heat.rs:17` is the struct default and is never what runs:
`Config::load` applies the preset unconditionally (`src/config.rs:104`,
`:151`) and `Preset::apply` is the only writer of `heat.half_life_secs`
(`src/config_preset.rs`). The shipped default is `relaxed` = **30 days**; medium
is 7, tight is 3. So the two signals are 24h vs 30d by default, and the gap to
the derived 1–2 days is a factor of 15–30, not 3.5. The knobs also stopped being
config edits — a retention retune is now a commit against `preset.rs`, which is
item 87's surface, and the two should be swept together. Now measurable:
`tests/e2e/test_recall.py` scores a half-life change directly
(`recall@1`/`recall@5`/`MRR`), which is the sweep that was never run.

**Measurement half-closed 2026-07-23 — both half-lives now surfaced.** The
retention half-life shipped in the item 62 surfacing half (`heat_half_life_secs`
in `Server::health_stats` JSON + `kern health` `heat: half-life` + `HealthRes`
serde default + `kern://local/health`). The **QBST recency half-life**
(`qbst_recency_half_life_secs`, `src/config_retrieval.rs`, 24h default) now
surfaces the same way: `HealthRes.qbst_recency_half_life_secs` `#[serde(default)]`
(old daemon → `0`), `Server::health_stats` JSON line, `kern health` `recency:
half-life {N}s` **daemon-sourced only** (item 100 rule — the daemon's running
preset is what the operator asked about), `kern://local/health` by construction.
Proved by `kern_health_prints_heat_half_life` (extended: asserts both
`recency: half-life 0s` + `86400s`), dto round-trip `86400` + old-payload
absence → `0`. `cargo test -p kern --lib` 957 passed (1 pre-existing
`the_sink_waits` flake, green isolated); `cargo test -p trnsprt --lib` 61 passed.
Decided by fix-the-root (surface both preset-owned signals, do not tune),
name-the-tradeoff (the recency companion to the heat line; tuning is the
bench/sweep, item 55/87, avoided), verify-before-claiming (negative control).
See the 2026-07-23 CHANGELOG entry.

**Still open:** the tuning sweep (item 55/87) — neither half-life has been
measured against recall.

---

# Tier 7 — belief model

`decisions/bayesian-confidence.mdx` and `decisions/edit-convergence.mdx`. None
funded before now. Ranked here because the model is coherent and merely
incomplete — no item below produces a wrong answer today.

### 56. An agent cannot register disagreement at all `[ingest]`

There is no `Contradicts` reason kind (`src/base_types.rs:90-99`) and no `stance`
parameter on the ingest schema (`src/mcp_tools_mutate.rs:19-33`);
`observe_contradict` (`src/base_types.rs:417`) has exactly one caller, GNN
alignment (`src/tick_gnn_propagate.rs:233`). Observer-reputation weighting is
also unbuilt.

### 57. No evidence decay — mechanism half-closed 2026-07-22 (default-off) `[lifecycle]`

**Mechanism half-closed 2026-07-22, default-off; policy decision still open.**
`decay_evidence` (new pub fn, `src/tick_stigmergy.rs`) γ-damps
`conf_alpha`/`conf_beta` toward the Jeffreys prior `(1,1)` by a half-life, gated
by `EVIDENCE_HALF_LIFE_SECS` (new, `src/base_constants.rs`, default `0` =
disabled = bit-identical today). For each non-superseded resident entity:
`conf_alpha = 1.0 + heat::decayed(conf_alpha - 1.0, updated_at, now, half_life)`,
likewise `conf_beta`, then `refresh_score()`. Decaying `(α-1)`/`(β-1)` toward 0
keeps `(1,1)` as the floor and never crosses it; `heat::decayed` is the existing
exponential. `run_gc` calls it gated by `EVIDENCE_HALF_LIFE_SECS > 0` (GC
cadence, hourly). `observe_support`/`observe_contradict` (`src/base_types.rs`)
now stamp `self.updated_at = Some(SystemTime::now())` so **every** conf change
tracks a timestamp (previously only the dedup caller did, so a decay using
`updated_at` would mis-read GNN-updated conf as stale); the redundant
`accept.rs:153` stamp removed. `updated_at` is existing federated state
(`merge.rs` `join_max_time`) — **no schema/wire change**; the stamp broadens its
meaning from "text changed" to "mutated", which decay needs. Local-only mutable
state (`merge_entity_never_imports_replica_local_mutable_state`), no
gossip/wire. Proved by `evidence_decay_damps_alpha_beta_toward_prior_by_half_life`
(α=11 β=3, `updated_at = now-7d`, half-life 7d → α≈6.0 β≈2.0, score refreshed),
`evidence_decay_half_life_zero_is_a_noop`, `evidence_decay_skips_superseded_entities`,
`observe_support_and_observe_contradict_stamp_updated_at`; existing
`dedup.rs:121` assert green unedited. `cargo test -p kern --lib` 942 passed, 0
failed, 4 ignored. Negative control: `decay_evidence` early-return reds the
damps test, green on revert. Decided by fix-the-root (ship the mechanism, not
the policy), name-the-tradeoff (default-off — today unchanged; the rate question
`bayesian-belief.md:161` is the operator's; a future on-by-default must
re-measure recall since `conf_variance` shrinks under decay and item 65's
lower-bound ranking moves), verify-before-claiming (negative control). See the
2026-07-22 CHANGELOG entry.

**Still open:** the policy decision — enable-by-default and the rate (per-tick /
per-day / access-tied).

~~`conf_alpha` and `conf_beta` only grow — the sole zeroing is the remote strip
(`src/merge.rs:28-29`) — so stale consensus takes proportionally many new
observations to unseat. Tick-based γ damping is an open design
(`decisions/bayesian-confidence.mdx:137`).~~

### 58. Supersede chains are unbounded while contested — trigger #1 instrumented 2026-07-22 `[lifecycle]`

**Trigger #1 instrumented 2026-07-22; rate-limit / `ReasonKind::Edit` decision +
triggers #2/#3 still open.** Trigger #1 ("supersede chains routinely exceed ~5
hops on the same `external_id`") is now detectable: `supersede` /
`supersede_by_contradiction` (`src/accept.rs`) increment a process-global
`SUPERSEDE_CHAIN_DEPTH_EXCEEDED` `AtomicU64` when the chain depth (via the
existing `superseded_ancestors` walk) exceeds `SUPERSEDE_CHAIN_HOP_THRESHOLD`
(new, `src/base_constants.rs`, default `5` — the doc's own number). The counter
reads into `HealthStats.supersede_chain_depth_exceeded`, folds into the `kern
health` `degraded:` line (daemon-sourced only, item 100 rule, item 28
`gnn_train_refused` precedent), and rides MCP `health` JSON +
`trnsprt::HealthRes` `#[serde(default)]` (old daemon → `0`, same shape as
`gini_access`/`max_kerns`). Proved by
`supersede_chain_depth_counter_increments_past_threshold` (6-deep chain →
delta 1; 3-deep → 0; serialised on `SUPERSEDE_CHAIN_TEST_MUX` per the item 28
lesson — a process-global moved by one test races another that measures it),
`graph_health_stats_carries_supersede_chain_depth_exceeded`, dto round-trip
`supersede_chain_depth_exceeded: 22`. `cargo test -p kern --lib` 952 passed, 0
failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed. Negative control
(`SUPERSEDE_CHAIN_HOP_THRESHOLD = usize::MAX` → no increment) reds, green on
revert. Decided by fix-the-root (measure, do not act — the rate-limit/`Edit`
kind is the decision, out of scope), name-the-tradeoff (trigger #1 only; #2
needs per-`external_id` producer tracking — stateful, bigger; #3 is a user
request, not instrumentable), verify-before-claiming (negative control +
process-global serialisation). See the 2026-07-22 CHANGELOG entry.

**Still open:** rate-limit / `ReasonKind::Edit` + schema (the decision); trigger

# 2 (producer ping-pong) + #3 (byte-level blame request) instrumentation

~~No `ReasonKind::Edit` rationale edge (`src/base_types.rs:90-99`) and no producer
rate-limit, so an A/B ping-pong on one `external_id` grows without bound
(`decisions/edit-convergence.mdx:107`). Compounding it: the three trigger
conditions that would flip kern to full versioning have **no instrumentation**
(`docs/wikipedia-edit-convergence.md:100-105`), so the flip is undetectable
even in principle.~~

### 59. `degrade` has no floor, no audit trail and no undo — floor half-closed 2026-07-22 `[retrieval]`

**Floor half-closed 2026-07-22; audit trail and undo still open.**
`degrade_entity_reasons` (`src/commands_graph_ops.rs`) now clamps
`r.score = (r.score - decay).max(DEGRADE_FLOOR)` with `DEGRADE_FLOOR` (new,
`src/base_constants.rs`, default `0.0`), so a surviving reason score never goes
below the floor. **Honest gap, named:** under current constants
`DEGRADE_MIN_THRESHOLD` (0.05) > `DEGRADE_FLOOR` (0.0), so `should_remove` fires
(`score - decay < 0.05`) and removes an edge *before* the else-branch decay the
clamp guards runs — an edge that survives has `score - decay >= 0.05 > 0.0`, so
`.max(0.0)` is a no-op on it at default. The clamp is therefore **defensive at
default**: it holds the invariant "no surviving reason score is below the floor"
and becomes live the moment a score arrives below the floor (e.g. a gossip merge
of a pre-floor-era negative value) or the threshold is lowered below the floor.
The item's premise ("score can go negative") is already guarded by the threshold
under the current code — there is one score-decrement site and it is always
preceded by the threshold check; the brief's negative control ("clamp removed →
score goes negative") is unwritable at default constants because removal
preempts the clamp. Verified the clamp is wired by temporarily raising
`DEGRADE_FLOOR` above the threshold (survivors clamp to the floor) and
reverting. Shipped test pins the invariant (post-degrade, every surviving reason
score `>= DEGRADE_FLOOR`); `degrade_decays_survivors_and_removes_below_threshold`
green unedited. `cargo test -p kern --lib` green. Decided by fix-the-root (clamp
the one decrement site, not every caller), name-the-tradeoff (defensive at
default — the invariant is the value, the live fix is conditional), verify-
before-claiming (the unwritable negative control was diagnosed, not faked). See
the 2026-07-22 CHANGELOG entry.

**Still open:** audit trail (durable record a degrade happened) and undo
(reverse op) — the halves that address "erasing a correct path"; the floor does
not stop removal below `DEGRADE_MIN_THRESHOLD`.

~~It lowers edge scores unboundedly (`howto/mcp.mdx:104-106`,
`concepts/why-kern.mdx:127-129`); nothing stops repeated degrades from
permanently erasing a correct path, and nothing records that they happened.~~

### 60. No re-classification when a contradiction pair changes `[lifecycle]` — closed 2026-07-22

Either side of a classified pair can move and nothing re-runs the call, and no
tool exposes the supersede chain beyond `include_history` (`FEATURES.md:173-174`).
Two open questions beside it, from `docs/bayesian-belief.md:159-162`: should
`Reason` edges carry belief symmetrically, and does superseding reset or inherit
belief?

**Belief half closed 2026-07-22 by decision; re-classification wiring stays
open.** The two `bayesian-belief.md` questions are settled, both conservatively
to the behaviour already shipped:

- **Edges carry belief directionally, not symmetrically.** A `Reason` edge is
  evidence `from` holds about `to` — `Provenance` (a session justified a claim),
  `Question` (an entity asks another), `Supersedes` (new replaces old) are
  directional by construction. Making belief symmetric would conflate "A vouches
  for B" with "B vouches for A" and erase the one-direction `traversal_count`
  GCounter a walk already reads. The CRDT `score` on an edge stays the one
  symmetric field (LWW-convergent), and belief stays on the entities.
- **Superseding resets belief, it does not inherit.** A supersede is a *new*
  claim with its own text; the new entity mints its own beta prior from its
  minted confidence (`beta_params_from_confidence`). Inheriting the old
  entity's `conf_alpha`/`conf_beta` would fold accumulated evidence for a
  statement nobody makes anymore into a statement just minted — a single
  observation would read as well-evidenced. Reset is the current behaviour and
  the decision; `observe_support`/`observe_contradict` accrue on the new entity
  from its own later corroboration.

What stays open is the item's title: when one side of a classified pair moves
(an entity carrying a deferred `Rephrase` candidate is itself superseded),
`do_classify_contradiction` returns early on `old.is_superseded()` and the
candidate is never re-classified against the new active entity.

**Re-classification wiring closed 2026-07-22.** `stamp_superseded` (the shared
supersede tail in `src/accept.rs`) now walks the old entity's deferred
`Rephrase` edges (`kind == Rephrase`, `to` empty, `from == old_id`), re-points
`from` to the new active entity, and pushes `(kern_id, reason_id)` onto a new
`GraphGnn::pending_reclass` set (`parking_lot::Mutex<Vec<_>>`, the
`pending_deltas` pattern). The tick loop (`src/tick.rs::start`) drains
`drain_pending_reclass` each iteration and re-enqueues
`ClassifyContradiction` via `task_extra`, so the candidate re-classifies the
new entity's text against the deferred wording on the next tick. A crash or
restart drops the pending set (the Rephrase is already re-pointed on disk via
the supersede's persist, so a restart's `do_classify_contradiction` loads the
re-pointed `from` and classifies anyway — the queue is an acceleration, not a
correctness dependence). New test `supersede_repoints_a_deferred_rephrase_to_
the_new_entity` pins the re-point + queue. Decided by: name-the-tradeoff,
verify-before-claiming, fix-the-root.

---

# Tier 8 — retrieval quality, now measurable

These were unacceptable-in-principle until an instrument existed. It does now
(`tests/e2e/test_recall.py`, and the invariants beside it), so each of these can be
judged the way item 86's two candidate fixes were: apply it, measure, keep it only
if `recall@1` holds and the exact-match probe stays at rank 1. They are ranked
among themselves by expected effect, not by confidence.

What the instrument still cannot judge, and what that costs: the fake embedder is
bag-of-words, so a change that only a real embedding model would reward looks
neutral here. Item 64's stemmer swap is the clearest case.

### 103. Run the public retrieval benchmarks and record the first numbers — closed 2026-07-23 `[bench]`

The harness exists (`tests/e2e/eval/`, CHANGELOG 2026-07-22): LoCoMo-10 and
LongMemEval-S scored retrieval-only — recall@k / MRR / NDCG against the
datasets' own evidence labels, no LLM anywhere in ingest, retrieval, or
scoring — exactly the replacement the 2026-07-20 deletion entry promised.

**LoCoMo-10 half done, measured 2026-07-22** (`qwen3-embedding:0.6b` on
Ollama, direct path, k=10, 1536 questions, 446 adversarial + 4 evidence-less
excluded, 0 truncation collisions; report
`eval/reports/locomo-20260722-081539.json`, CHANGELOG entry same day):
recall_any@1 **0.3092**, recall_any@5 **0.5983**, recall_any@10 **0.7129**,
recall_all@10 0.5710, MRR 0.4427, NDCG@10 0.4602. Per category (any@5):
temporal 0.6760, single-hop 0.6088, multi-hop 0.5532, open-domain 0.3696.
Query latency p50 0.62s / p95 0.68s — cold-process CLI wall clock, not a
serving-path number. The one peer in this protocol family, YourMemory,
publishes LoCoMo-10 Recall@5 0.59 (vendor-run, hit-definition unstated);
kern's any@5 0.5983 sits at parity with that claim, and no stronger sentence
than "parity with the one vendor-run peer number" is licensed. Zep's 75.14 and
Mem0's 92.5 are LLM-judged end-to-end scores and are not comparable.

**LongMemEval-S half done, measured 2026-07-23** (`qwen3-embedding:0.6b` on
Ollama, direct path, k=10, seeded 100-question sample of 500 — seed 13, the
runner's default — 4792 sessions ingested, 196 label collisions resolved
through `kern get`; report `eval/reports/longmemeval-20260723-193342.json`,
CHANGELOG entry same day): session granularity recall_any@1 **0.83**,
recall_any@5 **0.97**, recall_any@10 **0.99**, recall_all@10 0.91, MRR 0.8896,
NDCG@10 0.8589 over 100 questions; turn granularity (93 questions carrying
`has_answer` labels) recall_any@1 0.5054, recall_any@5 **0.8065**,
recall_any@10 **0.9032**, recall_all@10 0.6989, MRR 0.6271, NDCG@10 0.6224.
Per type (session any@1): single-session-assistant 1.00, knowledge-update
0.944, multi-session 0.80, temporal-reasoning 0.792, single-session-user 0.75,
single-session-preference 0.429 (n=7 — the weakest, same finding shape as
LoCoMo's open-domain). Query latency p50 0.53s / p95 0.63s, cold-process CLI
wall clock. Honesty notes: a 100-of-500 seeded sample, not the full set —
`--full` remains for anyone wanting the whole-set number; the release binary
was rebuilt mid-run with a `kern get`-only provenance addition (`cc37be8`),
which the query path never executes. LongMemEval's published numbers are
LLM-judged QA accuracy and are not comparable to these retrieval-only ranks.

Both halves are now recorded; closed. Follow-ups live elsewhere: the weakest
categories (LoCoMo open-domain 0.3696 any@5, LME single-session-preference
0.429 any@1) are findings for Tier 8, and the daemon-mode LoCoMo bench is item
104's still-open half.

### 104. Benchmark the full pipeline, which needs turn-level claim provenance — ground half-closed 2026-07-23 `[ingest]`

**Provenance landed 2026-07-22 (`fd914a9`).** The blocker this item named is
gone: `distill` (`src/ingest_distill.rs`) splits the transcript into 1-based
turns (blank-line boundaries, the same unit `paragraph_split` and the LoCoMo
harness use), marks them `[i]` inline in the prompt, and parses an optional
`"turns": [<1-based numbers>]` array per claim (non-integer/<1 dropped, malformed
degraded to empty — never a panic). `drain_entry` (`src/ingest_intake.rs`)
comma-joins the cited turns into `Source::Session.section` — the existing empty
carrier, so no bincode schema change (repo law 1, append-only, held); uncited
claims keep `section` empty = byte-identical pre-provenance baseline. Proved by
`parse_claims_records_turn_provenance`, `turns_absent_or_malformed_leaves_empty`,
`split_turns_breaks_on_blank_lines`. Decided by: avoided-question (reuse the
empty `section` carrier, no new field — schema stays append-only),
name-the-tradeoff (1-based cite-by-position; a re-edited transcript shifts them
— accepted, distill runs once per archived transcript).

**Ground half-closed 2026-07-23 (see the CHANGELOG entry).** The distill path
is now scored, on kern's own committed corpus: `tests/e2e/eval/ground.json`
(self-authored CC0, 8 sessions / 82 turns / 34 questions, anchors CI-enforced
by `test_eval_ground.py`) + `run_ground.py` (`just eval-ground`), which runs
the same turn-level labels over the direct path (documents, verbatim floor)
and the intake pipeline (`kern intake drain`, real distill; claims map back to
cited turns via `kern get`'s new `Source:` provenance line). First numbers
(qwen3-embedding:0.6b, distill qwen3.5:4b, k=10, report
`ground-20260723-192745.json`): direct recall_any@10 **0.824** / MRR 0.407;
distill recall_any@10 **0.324** / MRR 0.259. The finding: the gap is ingest
coverage, not retrieval — 24 distinct claims retrieved for 82 turns, 17% of
hits cite no turns, single-hop worst (0.20 any@10) while temporal/update hold
0.43–0.60 because dated facts survive distillation. That makes the distill
prompt's claim yield the next lever (item 49's chunking half is adjacent), and
any future 'distill quality' change has a number to move.

**Still open — the LoCoMo daemon-mode bench.** An eval mode that ingests via
the daemon (so the tick pipeline runs — distill claims, reasons/edges, GNN) and
scores recall@k against LoCoMo's per-turn evidence labels, compared to the
0.7129 direct-path baseline (item 103). This is the number that measures the
real graph at LoCoMo scale and the path to a Zep/Mem0-comparable result: kern
retrieves top-k, an external LLM synthesizes, recall@k is what the synthesis
can reach. The ground corpus above is the same shape at 1/40 the size — the
protocol is proven, the scale run remains.

### 94. A near-duplicate's alternate wording is indexed and findable — closed 2026-07-22 `[retrieval]`

The premise held. Confirmed by measurement before any fix: a dedup of
`"alpha beta gamma"` and `"alpha beta gamma, restated"` leaves
`rephrases=[("alpha beta gamma, restated", 0)]` on the survivor —
stored, `vector.len() == 0` — with `reason_idx` empty and the lexical index
answering `[]` for `restated` while it answers the survivor for `alpha`. The
failing query that proves it is real: over a 21-entity corpus where 20 fillers
sit nearer the query vector, `velocipede outbuilding` — the merged-away
document's own words — returned twenty fillers and never the survivor.

**The fix the item proposed would have been wrong.** `LexicalIndex` is keyed by
entity id and `inner_insert` removes before it inserts, so "one `lex.insert` of
the rephrase text against the survivor's id" would have *replaced* the
survivor's own posting with the alternate's. Shipped instead: one lexical
document per entity, `entity_document` (`src/lexical.rs:15`), being the
entity's statements followed by every `Rephrase` text hanging off it. One
document per id is also the answer to "does the entity appear twice" — BM25
cannot return one id twice, so no dedup logic is needed at the seed layer.

Lexical only; **no dense vector for the alternate**. `merge_duplicate` is a pure
graph function reached from both gates and neither has an embedder in hand, so a
vector would make dedup an I/O operation on the write path, and item 83 already
names vectors as the largest single holder. It would also buy nothing on the
default path: only `Mode::Reason` seeds off `reason_idx` (`seed_by_reason`);
`Mode::Hybrid` never reads it. And the dense gap is the smaller one by
construction — the two texts merged *because* their vectors were within
`INGEST_DEDUP_THRESHOLD`, so the survivor's own vector already stands in for the
alternate. The gap was purely lexical: exact rare terms.

Lifecycle, so the wording cannot outlive its survivor: `reindex_entity`
(`src/lexical.rs:34`) re-derives the document from the graph and is called
at every site that mints or drops a `Rephrase` — `merge_duplicate`
(`src/accept.rs:187`), the supersede path that consumes one
(`src/tick_tasks.rs:205`), and `degrade_entity_reasons`
(`src/commands_graph_ops.rs:491`). GC needed nothing: `remove_entity` already
calls `lex.remove(id)`, and the wording lives in the survivor's own document.
`rebuild_from_graph` uses the same builder, or the posting would survive exactly
until the next reload.

Cost, named: the survivor's `doc_len` grows, so BM25 length normalization
(`b=0.75`) dilutes the primary wording's own terms a little. That is the trade
for the alternate being reachable at all.

**What it does not buy, and why the recall number did not move.** Recall is
unchanged at 0.9306 / 0.9722 / 0.9471 because the e2e corpus has no
near-duplicate pair, so no `Rephrase` is ever minted and every lexical document
is byte-identical to what it was. The corpus was deliberately left alone so the
baseline stays comparable. More importantly, a CLI-level probe cannot show this
today: `cmd_search` (`src/commands_query.rs:101`) is pure vector and never reads
the lexical index at all, and `kern query` runs `fuse_hybrid_seeds`, which
rescores every fused seed by `cosine(qvec, entity.vector)` — so a lexical-only
hit is re-ranked by exactly the signal that failed to find it and is cut
whenever the candidate pool overflows the delivery cap. The fix makes the
wording a *candidate* where it was not one; turning that into a delivered rank
is item 61's question, not this one.

### 61. Move `merge_hits` onto RRF `[retrieval]`

The raw `0.4·content + 0.6·GNN` blend (`src/search.rs:60-61`, applied `:74`)
is fragile across scales; `fuse::rrf` (`src/retrieval_fuse.rs:4`) already fuses
the seed layer (`src/retrieval_query.rs`, `fuse_hybrid_seeds`) and never reaches `merge_hits`.
**Record the cost:** RRF keeps rank information only, so a dense hit that is
overwhelmingly better than rank 2 gets no credit for the margin
(`concepts/retrieval.mdx:88`). This is a trade, not a strict win.

### 62. The self-organisation claim is unmeasured — half-closed 2026-07-22 `[retrieval]`

**Half-closed 2026-07-22 (Gini-over-access half; top-10 stability still open).**
The Gini-over-access convergence metric now exists and is surfaced in health,
so the central claim "the corpus converges on efficient paths" is measurable.
`gini_over_access(counts: &[u64]) -> f64` (new pure fn, `src/health.rs`)
is the standard Gini coefficient over entity `access_count.value()`: `0.0` =
uniform access (converged), → `1.0` asymptotically as one entity holds all
access (**for finite n the max is (n−1)/n**, so `[10,0,0]` → `2/3`, `[100,0]`
→ `1/2` — the asymptote is the direction, not a value any real corpus hits).
`HealthStats.gini_access: f64` (new, computed in `graph_health_stats` over
resident entities; empty graph → `0.0`; `HealthStats` dropped `Eq` since `f64`
is not `Eq`, no caller used it). MCP `health` JSON carries `gini_access`;
`trnsprt::HealthRes` gains `#[serde(default)] gini_access` so an old daemon's
payload reads `0.0` (same shape as `ingest_queue_depth`, item 30). `kern health`
prints `convergence: gini 0.NN` **daemon-sourced only** — the CLI's own graph is
a fresh open with no query history, so its access distribution is structurally
uniform and a local read carries no signal (item 30/100 precedent). Proved by
`gini_over_access_pins_known_distributions` (`[]`/`[5,5,5]`/`[1,1,1,1]`/`[0,0,0]`
→ `0.0`, `[10,0,0]` → `2/3`, `[100,0]` → `1/2`),
`graph_health_stats_empty_graph_gini_is_zero`,
`graph_health_stats_skewed_access_gini_above_half` (one hot + five cold → `5/6`
> `0.5`), and the trnsprt round-trip with `gini_access: 0.42`. `cargo test -p
kern --lib` 920 passed, 0 failed, 4 ignored; `cargo test -p trnsprt` 60 passed.
Negative control: `gini_over_access → 0.0` always reds the skewed test, green on
revert. Decided by fix-the-root (compute, do not act — item 54's gate is a
separate decision), name-the-tradeoff (finite-n Gini maxes at (n−1)/n, stated
not hidden), verify-before-claiming (negative control). See the 2026-07-22
CHANGELOG entry.

**Still open:** top-10 stability (needs two temporal query-ranking snapshots —
stateful, separate slice); ~~`kern://health` resource surfacing~~ **— surfaced
2026-07-23** (see below); item 54 GC convergence gate (depends on this +
stability).

**`kern://health` HeatStats surfacing closed 2026-07-23.** The active heat
retention half-life (`HeatConfig.half_life_secs`, the one `Preset::apply` sets —
relaxed=30d / medium=7d / tight=3d) is now surfaced: `Server::health_stats`
(`src/mcp.rs`) JSON carries `heat_half_life_secs` from `self.cfg.heat`;
`trnsprt::HealthRes` gains `#[serde(default)] heat_half_life_secs` (old daemon →
`0`); `kern health` prints `heat: half-life {N}s` **daemon-sourced only** (item
100 rule — the CLI's own config is irrelevant, the daemon's running preset is
what the operator asked about); the `kern://local/health` resource carries it by
construction (`resource_health` → `server.health_stats()`). Proved by dto
round-trip `2592000` + old-payload absence decodes `0`, and
`kern_health_prints_heat_half_life` (30d → `2592000s`, `0` → `0s`, no daemon →
no line); negative control (omit the field → `HealthRes.heat_half_life_secs == 0`
→ print line reds, green on revert). `cargo test -p kern --lib` 955 passed, 0
failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed. Decided by
fix-the-root (surface the preset's retention, do not tune it — tuning is item
55/87), name-the-tradeoff (one knob — the retention half-life; the QBST recency
half-life is a separate ranking signal, item 55), verify-before-claiming
(negative control). See the 2026-07-23 CHANGELOG entry.

~~The convergence metrics — Gini over access, top-10 stability — were never built
(no `gini` anywhere in `src/`), so "the corpus converges on efficient paths", a
central product claim, is a design intention
(`decisions/stigmergy-over-gardening.mdx:128`). Belongs with item 1's
replacement. Its surfacing half is separate and also unbuilt: export `HeatStats`
via health and `kern://health`
(`docs/stigmergy-self-improving.md:271`). Item 54 depends on this.~~

### 64. Normalize and re-found the scoring stack `[retrieval]`

Three that must be judged together, since each moves the others:
min-max normalize `apply_boosts`, which is purely additive and unnormalized today
(`score * confidence + boost + fact_bonus`, `src/retrieval_score.rs:94-109`); swap
the hand-rolled stemmer (`src/lexical.rs:244`, no stopword list, no
`rust-stemmers` in `Cargo.toml`) for `rust-stemmers` 1.2.0 + stopwords, which
needs a BM25 rebuild; and validate-or-remove GNN reranking, whose only expression
is the 0.6 blend in item 61.

### 65. Rank on the lower confidence bound — closed 2026-07-22 `[retrieval]`

**Closed 2026-07-22.** `apply_boosts` (`src/retrieval_score.rs:131`) now ranks
by the lower confidence bound `conf_mean() − K·√conf_variance()` (clamped
`>= 0.0`), not the mean, so a single-observation claim stops outranking a
well-evidenced one at equal mean. `CONFIDENCE_BOUND_K` (new,
`src/base_constants.rs:76`, default `1.0` = one standard deviation, the
natural Wilson-style bound) is a tunable constant like the other knobs, not a
product choice. `e.score` stays the mean everywhere else (routing, merge,
storage) — only the ranking confidence factor moved to the lower bound, the
one-line change the item names. Proved by
`lower_confidence_bound_ranks_well_evidenced_above_single_observation`: two
entities at equal `conf_mean` (Beta(2,1) vs Beta(20,10), both 0.67), the
lower-variance (well-evidenced) one ranks higher; negative control at `K=0`
(or `+` for `−`) ties them. `cargo test -p kern --lib` 913 passed, 0 failed, 4
ignored. Decided by fix-the-root (move the ranking factor only, leave
`e.score`/routing/merge on the mean), name-the-tradeoff (`K` is a knob, default
1.0; auto-tuning is item 66), verify-before-claiming (negative control). See
the 2026-07-22 CHANGELOG entry.

~~`p − k·√var` instead of the mean (`docs/bayesian-belief.md:149`) — a
one-line ranking change that makes a single-observation claim stop outranking a
well-evidenced one at equal mean.~~

### 66. RRF weights and mode blends are configurable but never auto-tuned — measurement half-closed 2026-07-23 `[retrieval]`

**Measurement half-closed 2026-07-23; tuning sweep still open.** The active RRF
config (`rrf_k`, `rrf_global_weight`, the three `ModeWeights`
`weights_content`/`weights_reason`/`weights_hybrid`, each `{content, reason,
edge}`) is now surfaced: `Server::health_stats` (`src/mcp.rs`) JSON carries a
`retrieval:` block from `self.cfg.retrieval`; `trnsprt::HealthRes` gains a nested
`RetrievalHealth` `#[serde(default)]` (old daemon → zeroed); `kern health` prints
4 lines (`retrieval: rrf_k {}, global {}` + content/reason/hybrid weights)
**daemon-sourced only** (item 100 rule); `kern://local/health` by construction.
Proved by `kern_health_prints_retrieval_config` (4 lines, non-default values;
old daemon → 4 zero lines; no daemon → empty) and
`every_health_field_round_trips_through_json` (retrieval round-trip). `cargo
test -p kern --lib` 961 passed (1 pre-existing `the_sink_waits` flake, green
isolated); `cargo test -p trnsprt --lib` 61 passed. Standing negative-control
guard: old-payload-absence decode → `HealthRes.retrieval` zeroed (the print
block reds if the block is omitted). Decided by fix-the-root (surface the
configured RRF, do not tune it — tuning is the bench/sweep, item 55/87),
name-the-tradeoff (one block — the three `ModeWeights` carry the blends; the
recency/heat half-lives are separate signals, items 55/62), verify-before-
claiming (old-payload-absence guard). See the 2026-07-23 CHANGELOG entry.

**Still open:** the tuning sweep — RRF weights + mode blends never measured
against recall.

**Completion 2026-07-23 — remaining `RetrievalConfig` knobs surfaced.** The RRF
block shipped above carried the weights + blends; the four other active knobs
(`seed_k`, `mmr_enabled`, `lexical_enabled`, `pagerank_enabled`) were still
invisible. Now in the same `retrieval:` JSON block + `HealthRes` + `kern health`
(one line `seed_k N, mmr {bool}, lexical {bool}, pagerank {bool}`,
daemon-sourced) + RPC map. `kern_health_prints_retrieval_config` extended (5
lines); dto round-trip `seed_k 30/mmr false/lexical true/pagerank true`;
old-payload-absence decodes to `0/false` (standing guard). `cargo test -p kern
--lib` 964 passed, 0 failed, 4 ignored; `cargo test -p trnsprt --lib` 61
passed. Decided by fix-the-root (complete the block, one shape), name-the-
tradeoff (one line — the four knobs; tuning is the sweep), verify-before-
claiming (old-payload-absence guard). See the 2026-07-23 CHANGELOG entry.

~~Was two ceilings; the rerank half left with the rerank stage itself
(2026-07-21). What remains: RRF weights plus mode blends are configurable but
never auto-tuned (`FEATURES.md:210`).~~

### 67. Binary quantization stays non-user-selectable `[retrieval]` — closed 2026-07-22 by decision

Its recall floor is too low without a rescoring pass; deliberately excluded from
`parse` (`src/quant.rs:20-21`). Beside it: no int4 path and the quantization
scale is fixed at encode time (`FEATURES.md:265`).

**Closed 2026-07-22 — decided, not built.** Binary quantization stays
non-user-selectable. Exposing it through `QuantizationMode::parse` would let a
config opt into a recall floor the product has not measured against a rescoring
pass, and kern claims no retrieval quality it has not measured (item 1 / item
103). The `int8` path is already user-selectable (`kern compress`); `binary`
is kept off the parse surface until a rescoring pass (re-rank the binary
shortlist against the decoded float vectors) ships and the recall floor is
measured — a separate, larger item, not this one. The encode-time scale and the
missing int4 path stay as `FEATURES.md:265` records them. Decided by:
verify-before-claiming (no unmeasured recall knob on a surface a user can set).

### 69. Speculative decode for the distill leg `[ingest]`

qwen3.5:0.8b draft → 4b generator. With the answer leg gone (2026-07-21) the
only LLM latency that matters is distillation throughput; no `draft` or
`speculative` anywhere in `src/llm.rs`. Latency is the one axis item 1 does not
gate — the e2e harness can still judge this.

---

# Tier 9 — process, packaging, and things that rot unnoticed

Last because none of it affects a running kern. First within its tier because a
contract nobody enforces is a contract nobody has.

### 93. Line anchors cannot survive a merge, and `docs-check` cannot see it `[process]`

Every citation of the form `` `FEATURES.md:422-423` `` is a bet that nothing is
ever inserted above line 408. `FEATURES.md` only grows, so the bet loses on
every merge that appends — and when two branches each append and then combine,
it loses twice over. Four times on 2026-07-21: four anchors, then four more,
then twenty-seven, then fifteen. `tests/docs_check.py` was green through all
of it, correctly — it verifies the line *exists*, and it always does. <!-- docs-check: anchor-ok -->

The repeated hand re-pointing is not the fix. It is a tax paid on every merge,
by whoever remembers, and the first time nobody remembers the docs quietly start
lying again. Two candidate closures:

- **Symbolic anchors.** Cite a heading or a distinctive phrase
  (`` `FEATURES.md#12-mcp-surface` ``) and let the checker resolve it. Immune to
  insertion; breaks loudly on rename, which is the right failure. **Still open** —
  this is the better answer and the larger change, and nothing below replaces it.
- **Content-checked anchors.** ~~Keep line numbers but have `docs_check.py`
  verify the target still relates to the citing sentence.~~ **Landed 2026-07-21.**

**What landed.** `tests/docs_check.py` now compares the content words of the
citing block — the whole bullet or paragraph, since the docs wrap at eighty
columns — against the content words of the cited line(s), and nominates an anchor
whose target shares too few. Tokens are lowercased, split on `_` and on the
camelCase boundary, kept at four characters or more, and filtered through a small
stopword set. The bar depends on what is cited: two shared words for prose
targets, and for code targets only total silence counts, because prose citing
code shares almost nothing *by design* — the sentence explains, the line
implements. On the tree as it stands a two-word bar everywhere nominates 117 of
654 anchors and is unusable; the split bar nominates 39.

**It nominates, it does not gate.** Nominations print under their own heading and
leave the exit code alone; `python3 tests/docs_check.py` still exits 0 with 38
standing. `--strict-anchors` makes them fatal, for a CI that has decided to trust
them — not yet. A nomination a human has adjudicated is silenced in place with a
trailing `docs-check: anchor-ok` comment, the same idiom as the historical page
marker, and the marker counts only outside backticks so a page may quote it. The
paragraph above is the first user of that escape: its `FEATURES.md:422-423` is an
illustration of the disease, not a citation, so it is acquitted rather than fixed
— and so is this sentence, which repeats it. <!-- docs-check: anchor-ok -->

**Measured false-positive rate: 13 of 39, about 33%.** Adjudicated one at a time
against the real tree, not estimated. The 26 true ones are the expected shape —
`FEATURES.md` citing `classify_prompt` at a `return Vec::new();`,
`bayesian-belief.md` citing `conf_alpha`/`conf_beta` at a `ChunkPart` struct,
`README.md` citing the cold-tier drop counter at a closing brace; five of the six
anchors in one `crdts-federation.md` status block are wrong. The 13 false ones
share one weakness: the anchor is right but the target's distinguishing word is
under four characters (`acl`, `rrf`, `run_hub`) or is a stem the prose inflects
(`fn stem` against "stemmer"). Two more are self-inflicted — this item quoting
`FEATURES.md:422-423` as an example rather than as a citation — and are acquitted
in place, leaving **38 standing, 12 of them false, 32%**. <!-- docs-check: anchor-ok -->

**One measured non-win, recorded rather than buried.** Splitting the camelCase
boundary looks like a strict improvement and is not: it silenced two false
positives and one *true* one. `bayesian-belief.md:16` cites `src/base_types.rs:66-75`
for "the seven kinds that exist" while 66-75 is `EntityStatus` and `ReasonKind`
starts at 76 — a real breakage that now matches on a stray "entity" and no longer
nominates. Precision moved 64% → 67% and recall moved 27 → 26. It is kept because
prose and code should tokenise alike, not because the numbers earned it.

A third of the output being wrong is why `--strict-anchors` is opt-in, and it is
the number to beat before CI adopts it: stemming and a three-character floor with
a longer stopword list are the obvious next moves, and symbolic anchors retire the
question entirely.

**Second pass, 2026-07-21 — the nominations were worked, and both next moves
landed.** All 38 were opened at the cited line and adjudicated one at a time: 27
real drift, 11 false. Every one of the 27 was re-pointed and the new target read
back before it was written down — 29 anchors moved in all, counting the two bare
`` `:533` ``/`` `:398` `` continuations the regex never sees. Three of the false
ones were acquitted in place; the other eight the tokeniser can now see for
itself. `tokens()` keeps three characters instead of four, runs a light suffix
stripper with consonant-undoubling so `stemmer` and `stemmers` both reach
`fn stem`, and carries a stopword list grown to match — articles and pronouns are
back in play at three characters, and Rust's boilerplate (`let`, `pub`, `self`,
`new`) is there because a match on it says only that the target is Rust.

**Measured on the real tree, before and after, against a hand-adjudicated truth
set.** Precision 27/38 = **71.1% → 26/29 = 89.7%**. False-positive rate **28.9% →
10.3%**. Recall, over the 28 breakages known to exist, **27/28 = 96.4% → 26/28 =
92.9%** — *it went down*. The three-character floor that acquits `acl`, `rrf` and
`hub` also acquits `gpu`, and `gpu` plus `kern` is enough to reach the two-word
prose bar and silence a genuinely under-covering anchor (this file citing
`FEATURES.md:598` for three claims that span 589-590). That is the same trade the <!-- docs-check: anchor-ok -->
camelCase note above records, paid a second time, and it is recorded here for the
same reason. A prose bar of 1 instead of 2 measures 96.3% precision at unchanged
recall and was **declined**: the truth set holds only five prose-to-prose anchors,
which is not evidence, it is a sample small enough to fit anything.

**What remains.** Three false positives are structural, not tokenisation, and
carry the marker rather than a fix: `FEATURES.md:206` cites `if scored.len() < k {` <!-- docs-check: anchor-ok -->
for cold backfill and shares nothing with it because the line is a bound check
whose meaning lives in the lines around it; this file's two retired notes cite a
`README.md` table row and a `FEATURES.md` bullet for the single word each was
retired over (`move`, `acl`), one word short of the prose bar. Two of the three
would need a *block*-shaped target, not a better tokeniser. And the checker still
cannot see the `bayesian-belief.md:16` class at all — a wrong range that overlaps
the right one by a word — which is why that one was fixed by hand here rather than
by the tool. So `--strict-anchors` exits 0 on this tree, but **10.3% is not near
zero**: adopting it in CI buys a gate that will demand a human acquittal on about
one nomination in ten and will still go quiet on an off-by-a-range anchor.
Symbolic anchors remain the better and larger answer, and nothing here replaces
them.

**Third pass, 2026-07-22 — the checker was measuring 63% of the anchors and
reporting on all of them.** Both earlier passes tuned the *content* rule and
neither asked what the scanner could see. `REF` demands a literal `src/` prefix,
so two forms were invisible to it: a bare `` `:NNN` `` continuation, and a bare
`` `place.rs:112` ``. The docs use both constantly — a bullet names
`` `src/base_store.rs:624` `` once and then cites `` `:636` ``, `` `:649` `` and
`` `:684` `` rather than repeating the path — and across the scanned pages they
were **245 of 664 line anchors, 37%**, carrying no existence check and no
content check at all. The `store.rs` anchor there is an illustration of the
form, not a citation of the function, so it is acquitted in place rather than
re-pointed — the same verdict the `FEATURES.md` example above carries, and the
reason the doubly-backticked escape covers only the two forms added here.
<!-- docs-check: anchor-ok -->
The second pass felt this without naming it: it re-pointed "29 anchors in
all, counting the two bare `` `:533` ``/`` `:398` `` continuations the regex never
sees", by hand, because the tool could not.

Both now resolve against the last file cited before them, which is how a reader
resolves them, and the scope resets at every heading — a section is where a
reader stops carrying context forward. A bare name with no antecedent falls back
to a unique match under `src/`; `types.rs` is four files and `graph.rs` is three,
so an ambiguous one resolves to nothing and is reported rather than guessed. A
doubly-backticked span is a quotation of the form, not a use of it, so the
paragraph above displays `` `:533` `` without citing it. References checked, on
the tree as this pass found it: **834 -> 1008**; it reads higher now only
because this entry cites five things of its own. The remainder of the 245 live
in `CHANGELOG.md`, which carries the historical marker and is skipped whole.

**It found a dead reference on its first run, and the shape of it is the
argument.** This file cited `Drop for LocalListener` at `` `:654` `` under a
paragraph whose last named file was `client_local.rs` — 146 lines long. The
line number was right and the *file* was wrong: `Drop for LocalListener` is
`src/transport/src/typed/local.rs:654`. That is a failure mode a spelled-out path
does not have, and it is the one a continuation adds: existence stops being a
property of the anchor and becomes a property of the anchor plus everything
above it, so an edit that inserts an unrelated citation silently re-points every
continuation under it. Fixed here by spelling the path out, which is the only
fix that survives the next insertion.

**18 nominations, adjudicated one at a time against the real tree: 15 true, 3
false — 83.3% precision on a population that had never been checked.** Five of
the 15 are the wrong-file class the dead reference belongs to
(`` `:507` `` reaching `src/commands.rs` for a `bind_unix` that lives in
`src/transport/`, `` `:129` `` reaching `direct.rs` for a `Worker::submit` in
`file_watcher.rs`, and the two Windows/PQ notes at `` `:136-137` `` and
`` `:132-134` `` that say "doc-only" while landing in `src/graph.rs`);
the other ten are ordinary rot, including
`FEATURES.md:55` citing `Acl { scope, users, groups }` at a `_ => None,` thirteen
lines above the struct. The three false ones are the known weaknesses, not new
ones: `gc` is two characters and falls under the token floor, so the `gc` row
citing `fn tool_gc` shares nothing; and two are historical quotations of an
anchor being *discussed*. **They are reported, not fixed** — every one is a
`[surface]`, `[retrieval]`, `[lifecycle]` or `[federation]` claim, and this pass
owns `[process]`.

The remaining blind spot is unchanged in kind and now smaller in reach: symbolic
anchors are still the answer, and a continuation is the strongest argument yet
for them, because it is a line number that does not even name its own file.

**Verified independently 2026-07-22, against the commit rather than a working
tree.** The third pass reconciled, implemented and recorded in one go, so no
adversary had read it. Re-run: `just docs-check` green with its selftest,
`just check` green, `just test` 39 passed and 4 skipped with both recall floors
printed and unmoved. The 834 -> 1008 comparison reproduces — the prior script
over the prior tree reports 834 and nominates nothing, this one reports 1008 and
the single dead reference. The four-loops-into-one-sweep refactor is
verdict-identical: with the two new patterns neutered, the sweep prints
byte-for-byte what the prior build printed, so it moved no failure and no
nomination.

Two corrections to the paragraphs above. **The wrong-file class is four, not
five** — the two anchors reaching the wrong `src/` file and the two doc-only
notes; the fifth candidate is the retired `` `:189-192` `` note, counted among
the false ones above, which is the same class seen from the other side. And
**`--strict-anchors` no longer exits 0 on this tree**: 18 nominations stand, so
it exits 1. The second-pass sentence saying otherwise was true when written and
is superseded now.

**Re-adjudication does not reproduce 83.3%, and the gap is a definition rather
than a mistake.** Twelve of the 18 are rot by any reading. The three in dispute
are the wrong-file class, and for those the anchor a human resolves is
*correct*: `` `:507` ``, under a paragraph about `bind_unix`, means
`src/transport/src/typed/local.rs`, which is where `bind_unix` is, and `` `:129` ``,
under one about `Worker::submit`, means `src/ingest_file_watcher.rs`, which is
where the `submit` call is. Both hit the named symbol exactly. So the count is
15 if the question is "is this anchor under-specified" and 12 — 66.7% — if it is
"is the docs' information wrong". Either is defensible; the record should not
read as though only one measurement exists.

The distinction is load-bearing because the wrong-file class is the only one
that can turn the run *red*. It did exactly that once already, on this file's
own `Drop for LocalListener` note, and the fix was to edit the doc. Whenever an
inherited file is shorter than its inherited line, a page no human considers
wrong fails the check.

**One residual this pass opens and does not close.** Both new forms are matched
inside fenced code blocks, and inside any backticked span that merely looks like
a line number — a port, a ratio, a `sed` address. Either is a failure and not a
nomination once the number runs past the inherited file. There are none in the
tree today. The doubly-backticked escape covers prose and not fences; skipping
fenced blocks is the fix when one appears.

**Fourth pass, 2026-07-22 — the fenced-block residual closed.** `tests/docs_check.py`
now tracks ```` ``` ```` fence state per page and skips `REF` / bare-continuation /
bare-name matching inside a fenced block (heading-scope reset and the `GONE` skip
bypassed too — a `#` shell comment or `deleted` word in code is not a heading or
retirement). Guarded by a new `anchor_selftest` fixture: a real
`` `src/base_store.rs:624` `` citation before a fence, then a fenced block with
`` `:8080` `` (continuation past `store.rs`) + `` `graph.rs:9999` `` (bare name
past `graph.rs`); with the skip, 0 failures, total == 1; negative control (skip
disabled) reds on `` `:8080` `` as `store.rs:8080 beyond EOF`. **Honest finding —
the skip is NOT a no-op on the real tree, contrary to the third pass's "none in
the tree today":** three fenced `` `src/llm.rs:11434` `` tokens in
`docs/oracle/FEATURES.md:918` and `docs/oracle/ROADMAP.md:2916`/`:2925` were
being matched as dead references (`llm.rs` has 991 lines). With the skip they are
silent. Before: 3 dead references, 129 nominations. After: 0 dead references,
129 nominations — nomination count unchanged, three false positives fixed.
`python3 tests/docs_check.py --selftest` prints `selftest OK`. Decided by
fix-the-root (a fence is prose-as-code, not a citation — same reasoning as the
doubly-backticked escape), name-the-tradeoff (the skip is structural, not
tokenisation — it does not help a single-backtick `` `:8080` `` port outside a fence),
verify-before-claiming (the negative control reds, and the three real false
positives were found, not assumed). **Symbolic anchors remain open and the
better larger answer; this closes the fenced-block residual only.** See the
2026-07-22 CHANGELOG entry.

**The "After: 0 dead references" claim above was false on the real tree (2026-07-24).**
The three `` `src/llm.rs:11434` `` tokens are inline double-backtick illustrations,
not fenced blocks, so the fence skip never reached them — and the double-backtick
escape only covered `BARE_RS`/`CONTINUATION`, which ran on the blanked `quoted`,
while `REF`/`REPO_PATH`/`SIBLING_REF` still ran on raw `text`. So an illustrated
spelled-out path reddened as a phantom past-EOF citation and docs-check stayed red
from 2026-07-22 to 2026-07-24. Fixed 2026-07-24 by running every citation form over
`quoted`, so the illustration escape is uniform; the four surviving single-backtick
port tokens (`docs/oracle/FEATURES.md:925`, `docs/oracle/ROADMAP.md:2936`/`:3373`/`:3382`)
were escaped with the documented `` `:11434` `` idiom. `python3 tests/docs_check.py`
now exits 0; the selftest pins the escape and its negative control. See the
2026-07-24 CHANGELOG entry.

Neither is a defect in a running kern, which is why this sits in tier 9 — but it
is the reason every reconcile pass so far has spent most of its effort
re-pointing citations instead of checking claims.

**2026-07-24 re-anchor pass.** `FEATURES.md` had grown to 56 anchor nominations
(39 → 56 since this item was written 2026-07-21 — the debt compounding on
schedule); 22 were re-pointed to their verified definition line and the count
fell to 34. This changes nothing here: symbolic anchors remain the fix and this
item stays open. The pass is logged so the tax is visible, not so it reads as
progress toward closure.

**2026-07-24 re-anchor pass.** `FEATURES.md` had grown to 56 anchor nominations
(39 → 56 since this item was written 2026-07-21 — the debt compounding on
schedule); 22 were re-pointed to their verified definition line and the count
fell to 34. This changes nothing here: symbolic anchors remain the fix and this
item stays open. The pass is logged so the tax is visible, not so it reads as
progress toward closure.

### 98. The pre-auth frame is capped and deadlined — closed 2026-07-22 `[surface]`

Both halves were real and both are shut. 978 unit tests, e2e untouched.

**The size half was real by a different mechanism than this item named, and the
difference decided where the fix had to go.** There is no length field on this
wire to reject: `JsonEnvelopeCodec` is newline-delimited
(`src/transport/src/typed/codec.rs:53`), so nothing declares a size and
`FramedRead` simply reserves, reads and doubles its `BytesMut` for as long as
`decode` returns `Ok(None)` — which it does until a `\n` arrives
(`tokio-util-0.7.18/src/codec/framed_impl.rs:218`, `state.buffer.reserve(1)`
inside the read loop). So the allocation is not requested, it is accreted, and a
cap enforced *after* `channel.recv()` returns would never run at all, because on
this input `recv()` never returns. The cap therefore lives in the decoder, where
the buffer is, and measures an **incomplete** line: the only shape an endless
frame ever has.

Measured before it was fixed: a peer that writes 16 MiB with no newline had all
16777216 bytes taken into the daemon's buffer, and was then refused at EOF. The
refusal was already there. Only the memory was not bounded — which is why the
test asserts on bytes taken and not on the verdict
(`an_endless_pre_auth_frame_is_refused_without_being_buffered`).

**The patience half was real as written**, though "occupies its accept slot" is
not what happens: `serve_kern_rpc_loop` spawns per connection and keeps
accepting. What a silent peer holds is a task, an fd and a growing buffer, for a
session item 24 guarantees will never be authorised. Confirmed by removing the
deadline and watching a 20 s outer timeout fire instead of the inner one.

**The numbers.** `AUTH_FRAME_MAX = 1024` — a minted token is 64 hex characters
(`mint_token`), so a real frame is 110 bytes; 1 KiB is nine times that and an
order of magnitude under `FramedRead`'s own 8 KiB starting buffer, so the
refusal lands on the first decode. `AUTH_DEADLINE = 5s` — every real client
writes the token in the same breath as the connect, a microsecond conversation
over a local socket. Both are lifted the instant the frame is in hand, so the
authenticated path keeps the unbounded framing an ingest payload needs, and both
are therefore strictly tighter than anything past the gate.

**On timeout the connection is closed without a word**, which is the one place
this path departs from "one message for every refusal". A peer that ran out the
clock never spoke, so there is no misconfigured client to inform — and a reply
would be a free liveness probe that also names the deadline. An oversized frame
*does* get the standard refusal, deliberately: it did speak, and a distinct
answer would tell a caller which limit it hit.

### 96. A shared `target-dir` can report green on stale code `[process]`

The parallel-cycle worktrees all point `build.target-dir` at the main
checkout's `target/`, so one warm 11 GB cache serves every tree instead of each
paying a cold build. The cost was named when it was adopted — cargo takes an
exclusive lock, so concurrent builds serialize — but a second cost was not:
**under concurrent access a run can execute a lib-test binary that predates the
edit under test.** Observed 2026-07-21: a cycle saw `873 passed` with its own
three new tests absent from the run, and `touch src/lib.rs` changed the count to
a self-consistent 867.

**Corrected 2026-07-21, later the same day: it IS cross-worktree contamination,
and the first version of this item said the opposite.** That earlier paragraph
argued from the wrong artifact class. Each tree does get its own
`kern-<hash>` *lib-test binary* — four hashes for four trees — and that was
checked and is true. But a workspace sub-crate is a different artifact: `trnsprt`
has the same package name, version and relative path in every tree, so its
fingerprint matches across them and cargo reuses whichever build got there first.

Two independent observations, both loud:

- `cycle/3` failed to compile with `struct HealthRes has no field named
  ingest_queue_refused` — a field that existed **only** in `cycle/2`'s source
  tree. `cycle/3` was linking a sibling's `trnsprt`.
- `cycle/2` watched `cargo build` report `Fresh trnsprt` against a stale
  `libtrnsprt-*.rmeta` with **hard-link count 3** while its own `dto.rs` change
  sat on disk, in a tree that had just compiled cleanly under `cargo nextest`.
  Two rmetas carrying the field existed under different hashes (check and test
  profiles); the dev-profile one kept losing to sibling builds with a newer
  mtime. `touch src/transport/src/kern_rpc/dto.rs` before each build was the
  workaround it used for every number it reported.

So there are two failure modes, not one:

1. **Stale lib-test binary** — an aggregate count that looks green while the
   binary predates the edit. Fails *quietly*. **An aggregate pass count is not
   evidence that a new test ran.** The discipline that catches it: run the new
   tests BY NAME (`cargo nextest run --workspace -E 'test(<name>)'`) and read
   each result. A filter naming ten tests and printing ten results cannot be
   satisfied by a binary containing none of them.
2. **Foreign sub-crate rlib/rmeta** — one tree links another's `trnsprt`. Fails
   *loudly* here, because the two sources disagreed about a struct field. That
   is luck, not safety: a change that stayed type-compatible would have linked
   silently and produced a green run against a sibling's code.

**Decided: each worktree gets its own `target-dir`.** Three trees were flipped
the moment the second observation landed, and both stalled builds recovered
immediately. It costs a cold build per tree and roughly 33 GB against an 11 GB
shared cache — a straightforward trade once the alternative is "verification
results that may describe another branch's code". The staleness-guard
alternative (a `just` recipe failing when the binary is older than the newest
source) is now moot for the same reason: it would have caught mode 1 and been
blind to mode 2.

**The correct action is to delete the file, not to maintain a path.** Cargo's
default `target-dir` is already `<workspace-root>/target`, which in a worktree
is that worktree's own directory — isolation is what you get by doing nothing.
Confirmed on a tree with no `.cargo/` at all: it resolves to
`kern-cycles/2/target` unprompted. So the whole defect was introduced by adding
a `.cargo/config.toml` pointing at the main checkout, and the fix is to stop
writing that file when a worktree is created rather than to write it with a
different path. A per-tree path works but is a second thing to keep correct, and
the launch step that wrote the shared one is exactly the kind of step that gets
copied forward unexamined.

What survives: the by-name discipline. It is cheap, it catches mode 1
independently of the build layout, and it is what confirmed both cycles that
found this.

**Closed 2026-07-22 — the defect was already absent in this checkout, and now
that is recorded rather than inferred.** No `.cargo/config.toml` exists (verified:
`cat .cargo/config.toml` → none), `git worktree list` shows only the main
checkout (no cycle worktrees present), and cargo's default `target-dir` is
already `<worktree-root>/target` — which is the "do nothing" isolation the item's
ruling named as the correct fix. The cycle launch step that wrote the shared
`.cargo/config.toml` is gone alongside `~/.pi/agent/bin/` (the bin restructure
that dropped `cycle.sh`), so nothing currently re-introduces a shared target
dir. Negative control: `cargo build` in a fresh worktree with no `.cargo/`
resolves to its own `target/` unprompted (the property the item verified on a
tree with no `.cargo/` at all). The by-name discipline is the surviving residue;
it is process, not code, and stands as the item itself says — the catch that
works independently of layout.

### 97. The e2e harness runs the GNN and gates on it — closed 2026-07-22 `[eval]`

**Closed. The premise was right and understated: there were two independent
reasons no propagation ran, and only one of them was the corpus.**
`DEFAULT_MIN_THOUGHTS` is 128 (`src/gnn_propagate.rs:18`) and the recall corpus
is 36 facts (`tests/e2e/test_recall.py:199`) — but `test_recall.py` drives the CLI,
and **the CLI has no tick loop at all**. `do_gnn_propagate` is reachable only
from `tick::start` (spawned by `store::Registry::open`, i.e. a daemon or `kern
mcp`) and from `tick_sync`, whose one caller is a unit test. So the propagation
was not merely skipped at the threshold there; it was never called.

Both halves were measured through the real binary, with the daemon's own
propagation entry temporarily instrumented. At 36 facts under a daemon:
`entities=36 min=128`, entered and returned, nothing applied. **And at 150
facts: still nothing** — the boot cluster pass split the root into 36 + 114 and
neither part reached 128. That is structural, not incidental: `do_cluster`
enqueues `GnnPropagate` only `if did_structural_work`, and structural work is
the same act that moves entities *out* of the kern about to be propagated.

**So the closure the item favoured — grow the corpus — does not work at the
size it implies.** 150 facts leave the largest kern at 114, and the ingest cost
is already superlinear through the CLI: 1.9 s for 36 facts, 54.7 s for 150, each
`kern ingest` loading and re-saving the whole graph. Reaching a *post-split* kern
of 128 would cost minutes per e2e run and would depend on clustering behaviour
nobody has pinned — a gate that silently stops running the day that behaviour
shifts, which is the failure being fixed here, not a fix for it.

**What shipped is the other closure, plus the liveness assertion that makes it
worth having.** `tests/e2e/test_gnn_recall.py` writes an e2e-only `[gnn]
min_thoughts = 4` and `[tick] interval_secs = 0` (boot pass only, so the
embeddings are not still moving under the probes), starts a daemon with
`RUST_LOG=kern.gnn=info`, and **waits for the propagation to report itself**
before scoring anything. `do_gnn_propagate` now logs `learned propagation
applied` with a `nodes` count on success — failure was already loud, success was
silent, and `gnn_vector` is not persisted, so nothing on disk could have
answered "did it run". The test fails if no propagation arrives in 60 s, fails
if it covered fewer than 30 nodes, and only then scores recall.

Proof it is not another vacuous gate. With entity `i`'s propagated embedding
deliberately written to entity `i+1`: **`cargo nextest run --workspace` 972
passed, `tests/e2e/test_recall.py` passed printing its usual 0.9306 / 0.9722 / 0.9471,
and the new test failed 3 of 3 runs** (recall@1 0.7917 / 0.7222 / 0.7361 against
a 0.85 floor). Every existing gate was blind to a GNN wiring bug; this one is
not.

What is left, and it is what the item warned about: **the gate measures a
36-node graph.** It cannot catch a regression that only appears at scale, and
`tests/gnn_scale.rs` — the only thing that runs at 128+ — is `#[ignore]`d and
asserts nothing about ranking. Also unchanged: the propagation is stochastic
(unseeded weight init and negative-edge sampling), so its floors are looser than
the CLI corpus's and were set below the worst of 8 runs rather than from one.

### 92. Tests that race a backward-stepping `CLOCK_REALTIME` — closed 2026-07-22 `[eval]`

**Closed. Both tests were already fixed when this item last said one was not,
and the constant it recorded is wrong by 2.4x.** The mechanism holds:
`CLOCK_REALTIME` does step backwards here while `CLOCK_MONOTONIC` runs straight.
What it does not do is step "roughly 2.8 s every 30 s".

Measured two ways sharing no code path. A sampler reading both clocks every
50 ms and reporting every change in `realtime - monotonic`: over 300.009 s of
monotonic, **9 backward steps, mean -1.243 s, mean period 32.25 s**, 11.185 s
of realtime lost. A shell cross-check of `/proc/uptime` against `date(2)` over
120 s: 3.730 s lost, which is three steps of 1.243 s to a millisecond, reached
by arithmetic nothing in the sampler touches. A second 240 s window caught the
informative outlier: one period stretched to 47.45 s and its step grew with it,
to -1.816 s.

So **the rate is the invariant, not the step**. `1.243/32.25` is 3.85% and
`1.816/47.45` is 3.83%: realtime runs ~3.8% slow and the sync repays the entire
accrued drift in one jump whenever it fires. That reframes the item. A margin is not
unsafe because a fixed 2.8 s might land inside it; it is unsafe because the loss
scales with how long you wait, and a delayed sync bunches it.

It also explains the reproduction failures better than the old entry did. The
pre-fix shape — `time.sleep(RETENTION + 2)`, a 2 s margin over a 5 s retention —
was run 12 times interleaved with `test_recall.py` against today's rate and
passed **12 of 12**. One 1.24 s step cannot eat 2 s, and a 7 s window cannot
hold two steps that are 32 s apart. The six clean runs the original entry filed
as puzzling were the *expected* outcome; the two observed failures needed a
stretched sync interval, which the 47.45 s sample proves does occur.

Which means waiting for the flake is not a test. Reproducing it needs the step
**constructed**: an `LD_PRELOAD` shim over `clock_gettime` subtracting
`STEP × floor(monotonic / PERIOD)` from `CLOCK_REALTIME` alone. The offset is a
pure function of the monotonic clock, which is boot-relative, so the pytest
driver and every `kern` subprocess it spawns see one consistent warped realtime
while `CLOCK_MONOTONIC` stays untouched. At 2.8 s every 5 s the pre-fix shape
fails **5 of 5** on its original message — "an expired fact was still
delivered" — and the shipped shape passes **5 of 5** on that same clock, whole
file included.

**Both fixes are in and verified.** `src/ingest_intake.rs` waits on the wall
clock, restarts its marker on a backward step, and caps on the monotonic clock.
`tests/e2e/test_retention.py` waits to an *absolute* realtime target — which needs no
restart, since only realtime reaching the target can end the wait — and then
polls for the drop, because a step landing between the wait and the query can
put a passed deadline back in the future. It has done both since 2026-07-21
17:57 — seven hours before the 2026-07-22 00:37 commit that updated this item to
say it wanted the treatment. That sentence was written from the item rather than
from the file, which is the same reading-the-record-instead-of-the-thing mistake
that put 2.8 s in it.

An injected instant was considered and **declined**, on two counts. `kern query`
has no `--valid-at`; only the MCP surface does (`src/mcp_tools_query.rs`), so
the e2e harness cannot reach one without adding a CLI flag for a test's sake.
And it would measure the wrong path: `drop_expired` (`src/retrieval_score.rs`)
returns early whenever `valid_at` or `as_of` is set, leaving the work to
`matches_filter`, so an injected instant exercises the filtered reader while
production expiry rides the unconditional pass. Trading the real path for a
mockable one is no fix for a flake that never lived on the real path.

Nothing else here shares the defect. The only other test that sleeps and reads a
clock is `tick_head_of_line_delay` (`tests/gnn_scale.rs`), which measures with
`Instant`, and `tests/e2e/conftest.py`'s `wait_until` is monotonic on both sides.

What stands is why this was written down at all: **the next person to see it red
will assume it is this flake and wave a real regression through.** An
intermittent failure nobody has recorded is indistinguishable from a regression
nobody has noticed — and one recorded with a wrong constant is worse, because it
reads as adjudicated.

### 70. The oracle pre-commit hook is untracked and has no installer — closed 2026-07-22 `[process]`

**Closed 2026-07-22.** The oracle ruling hook is now tracked at
`hooks/pre-commit` (byte-identical to the legacy `.git/hooks/pre-commit`, 3049 B,
no logic change) and installed via a `just hooks` recipe that runs
`git config core.hooksPath hooks` (idempotent — re-run is a no-op), wired into
`.pi/update.sh` after the existing install lines so a fresh clone gets
enforcement after `pi install`. `core.hooksPath` supersedes `.git/hooks/`
entirely, so the legacy untracked file becomes inert and harmless — left in
place (no `.git/` write) as a fallback if someone unsets the path. Negative
control: with `core.hooksPath` unset and the legacy hook moved aside, a
throwaway `docs/oracle/ROADMAP.md` edit commits clean (gate absent); with
`just hooks` run, the same commit is refused by the oracle gate (rule 1
block, the new entry uncited). `just check` + `just docs-check` green; no Rust, no
hook-logic change, no new deps. Decided by fix-the-root (track + install the
hook the ruling already depends on, do not re-rule), name-the-tradeoff
(`core.hooksPath` supersedes rather than deletes — no `.git/` write, legacy stays
as fallback), verify-before-claiming (negative control: unset+no-legacy
commits, set+tracked blocks). See the 2026-07-22 CHANGELOG entry.

~~`ORACLE.md` rule 1 is enforced by `.git/hooks/pre-commit`, which lives only in
`.git/` and is created by nothing in the repo — no `justfile` recipe, no
`install.sh` step, no `.pi/update.sh` line. A fresh clone has **zero enforcement
of the ruling every commit is supposed to answer to**, and nothing announces it.
The hook itself calls this out as a "per-clone install product"; the install half
does not exist. Wanted: track it in the repo (`scripts/` was dissolved
2026-07-22; `.pi/` or a tracked hooks dir) and install it via
`core.hooksPath` or a `just` recipe run by `.pi/update.sh`.~~

### 75. Crash consistency on the DiskANN path `[store]`

**Half of this was verified false 2026-07-21 and the residual risk is narrower
than stated.** The item was written off `docs/diskann-disk-index.md:142-143`
("no WAL and no atomic-rename-per-segment") and never checked against the build.
Per-segment atomic rename *does* exist: `atomic_write`
(`src/diskann.rs:293-297`) writes `<path>.tmp` then `std::fs::rename`, and
`build_and_save` uses it for all three segments — meta (`:272`), vectors
(`:280`), graph (`:289`). `DiskIndex::open` (`:310-355`) then rejects a divergent
set rather than reading it: ids-length vs count, entry point in range, both mmap
lengths against the meta's `count × dim × 4` / `count × r × 4`, and every
adjacency slot either `SENTINEL` or a valid node id — the last one specifically
so a beam walk cannot slice the vector mmap out of bounds.

What is actually left is **cross-segment** atomicity, and it is a real hole: three
independent renames mean a crash between them leaves meta from build N+1 beside
vectors from build N. That survives the length checks whenever the two builds
have the same `count`, `dim` and `r` — the common case, since a rebuild usually
changes vectors and not shape. There is also no `sync_all` before the rename, so
rename ordering is atomic while the *data* behind it need not be durable. A
corrupt index is not fatal — `build_entity_disk_snapshot`
(`src/graph.rs:351-366`) logs and falls back to the in-RAM index — so this
is silent staleness, not a crash. Wanted: one rename that publishes all three
(a versioned directory, or a manifest naming the build the three files belong
to), and an fsync before it.

**Cross-segment atomicity closed 2026-07-22; fsync closed with it.**
`build_and_save` now builds into a staging dir (`<dir>.staging`), `atomic_write`
fsyncs each segment file (`sync_all`) before its rename, the staging dir itself
is fsync'd, and the publish is **one rename** of the staging dir over the live
dir (`std::fs::rename` after `remove_dir_all`). A crash before the swap leaves
the old build intact; a crash in the sub-microsecond window between
`remove_dir_all` and `rename` leaves no index, and `DiskIndex::open` failing is
non-fatal — `build_entity_disk_snapshot` logs and falls back to the in-RAM
index — so the worst case is silent staleness until the next rebuild, never a
mixed-build read. `rebuild_over_an_existing_index_swaps_and_leaves_no_staging`
pins two consecutive builds over one dir: the second is whole (its ids, not a
mix) and no staging dir lingers. POSIX-only safety is the tradeoff stated
here: `remove_dir_all` on a dir a reader has open mmaps of unlinks the inodes
but the reader's fd keeps them valid until close, so a search mid-rebuild
finishes on the old build; Windows cannot delete an open file, so a concurrent
reader would make the swap fail — DiskANN is off by default and Linux-first,
and the Windows mmap/lock semantics the item names separately stay open. The
PQ-codebook-drift and Windows-file-locking doc-only notes are unchanged.

Beside it, both unverified against source and still doc-only: mmap file-locking
and flush semantics differ on Windows
(`docs/diskann-disk-index.md:149-150`), and PQ codebook training/drift has
no retrain trigger — "a bad codebook silently degrades recall" (`:145-148`) —
which lands in item 1's lap the moment PQ is promoted out of the non-goals.

### 76. The watchdog force-exit attempts a bounded guarded flush and logs which happened — closed 2026-07-22 `[store]`

~~`spawn_watchdog` beats a counter once a second from the async runtime and
force-exits `std::process::exit(101)` after `STALL_LIMIT * CHECK_SECS` = 30s of
no progress. `process::exit` runs no destructor and no `Drop`, so it skips the
guarded shutdown flush the ordinary path takes — the `shutdown` notify at
`src/commands.rs:896` unwinds into the guarded persist closure `save_fn`
(`:633`, called at `:902`), which is the thing that "never overwrites a grown
disk". Nothing on the watchdog path wrote anything, and the exit line did not
say so.~~ **(The defect, closed 2026-07-22.)**

The stall it fires on is named as "graph deadlock or worker starvation", which is
precisely the state where the in-memory graph is ahead of disk and unreachable.
Combined with item 10 the default posture could lose up to a tick interval of
writes with no log. The awkward part is that a stalled runtime is exactly when a
flush may itself block, so "flush before exiting" is not free.

**Closed 2026-07-22 by the bounded attempt the item asked for.** `spawn_watchdog`
(`src/commands.rs:969`) now takes `save_fn` and is spawned after it is available
(`:722`), so a stall can attempt a flush before exiting. The stall branch calls
`watchdog_flush_attempt` (`:945`) — a testable free function that runs `save_fn` on
a dedicated thread and waits at most `FLUSH_DEADLINE_SECS` = 5s via
`mpsc::recv_timeout`, returning `WatchdogFlush::Flushed` (the guarded persist
landed) or `WatchdogFlush::Blocked` (it overran the deadline). The exit line names
which of the two happened before `process::exit(101)` (`:1010`) — the silence is
gone. A flush killed mid-write is safe because `atomic_write` is tmp+rename: the
live file stays intact and a detached flush thread that the process leaves behind
never finishes a partial write onto it. The watchdog starts after `save_fn` is
built rather than at process entry — before the graph loads there is nothing to
lose, so the later start costs nothing and buys the flush. Proved two ways:
`a_fast_save_fn_returns_flushed_and_ran` (a fast closure returns `Flushed` and
actually ran) and `a_save_fn_that_overruns_the_deadline_returns_blocked` (a closure
sleeping past 200ms returns `Blocked`, the watchdog did not block past the
deadline, and the closure did not complete inside the window). Decided by:
fix-the-root (flush on the watchdog's own thread, not the stalled async one),
name-the-tradeoff (the deadline is best-effort — a flush that cannot finish in 5s
on a dead runtime is hopeless, and the detached thread is reaped by the exit).
Supersedes: nothing — the item described the defect, now removed.

### 77. Hash composition is an unguarded breaking change — closed 2026-07-22 `[store]`

**Closed 2026-07-22.** Golden-vector tests now pin the exact pre-hash composed
string at each load-bearing identity-hash site, so a future composition change
breaks a test the same way a bincode schema change would. Four tests pin the
*shape* of what is hashed (not the digest of a value):
`source_id_pins_null_delimited_composition` (`Source::source_id`,
`ticket\x0042\x00disc`), `unnamed_kern_id_pins_parent_then_nonce_composition`
(`content_hash("p42")`), `named_child_kern_id_pins_parent_name_nonce_composition`
(`content_hash("pcode9")`), and
`structure_digest_pins_canon_layout_for_a_single_node`
(`content_hash("ep=a;max=0\na|\n")`, `HnswIndex::structure_digest` canon —
`=`, `;`, `|`, `\n` delimiters, empty-layer handling). The bare-text
composition (`content_hash(text)`) was already implicitly golden-pinned by
existing tests asserting an entity id equals `content_hash("hello world")`
exactly — not re-pinned. Negative control run per site (not shipped): reverting
one delimiter in each format string flipped the relevant test red, green on
revert. `cargo test -p kern --lib` 905 passed, 1 pre-existing env flake
(`config::tests::resolve_root_returns_start_when_no_kern_ancestor`, stray
`/tmp/.kern`). Decided by verify-before-claiming, name-the-tradeoff (pin the
pre-hash string, not the digest), fix-the-root (the determinism tests asserted
the property never at risk; the exact-string shape was the unguarded one).
Out of scope: a `HASH_VERSION` constant that versions the compositions — this
slice only adds the guard that makes a future bump loud. See the 2026-07-22
CHANGELOG entry.

~~"Changing how a hash input is composed is a breaking change to every existing
graph" (`concepts/graph.mdx:86-88`), and source confirms the exposure. The hash
itself is pinned — `content_hash` (`src/util.rs:3`) is sha256-to-lowercase-hex
and `util.rs:155` asserts length, alphabet and determinism. What is unpinned is
every *composition* feeding it, and each one is a different format string: entity
ids are `content_hash(text)` bare (`src/ingest_place.rs`,
`src/ingest_file_watcher.rs:181`, `src/ingest_direct.rs:39`,
`src/ingest_worker.rs:122`), `Source::source_id` is
`scheme \x00 object \x00 section` (`src/base_types.rs:254-266`), child and named
ids are `parent_id + nonce` and `parent_id + name + nonce`
(`types.rs:498-499`, `:503-504`), and the HNSW canon has its own
(`src/hnsw.rs:525`).

The bare-text composition is load-bearing beyond identity: the gossip import
guard re-derives `content_hash(&e.text()) == e.id` to refuse a forged remote
statement (`src/gossip_handler.rs:536`, and the note at `:521` names every minting
site it depends on). So changing that composition does not merely orphan old ids —
it makes every existing remote statement fail its receipt.

Repo law 1 guards bincode schema round trips; nothing guards or versions any of
these, and there is no migration path. Wanted: golden-vector tests pinning each
composed input string — not the digest of a value, the *shape* of what is hashed —
same standing as the bincode guard.~~

### 78. A non-local LLM URL egresses everything, silently — half-closed 2026-07-22 `[surface]`

**Half-closed 2026-07-22 (the "no warning at config load" half).** A non-fatal
warning now fires at config load when a configured `embed.url` or `reason.url`
points at a non-local host, so the one setting that voids "local-first, zero
egress" is legible the moment it is loaded. `is_local_url` (new, `src/llm.rs`,
pub) is the detector: loopback (`127/8`, `::1`, `localhost`), RFC1918
(`10/8`, `172.16-31/12`, `192.168/16`), link-local (`169.254/16`), the `ollama`
host, or the existing `is_local_ollama` heuristic (`` `:11434` `` / `//localhost` /
`//127.0.0.1`) — reused, not duplicated. No URL-parser dep: `host_of` extracts
the span between `//` and the first `:`/`/`/`?` (with a bracketed-IPv6 branch).
The WSL2-gateway the LoCoMo run used (`172.27.176.1:11434`) is RFC1918
`172.16/12`, so no false alarm. `Config::egress_warnings` (new, pure, no I/O)
returns one warning string per rejected URL — checks `reason.url` raw, not via
`reason_url()` fallback, so a silently-inherited `embed.url` does not
double-count. `boot_config` emits each via `tracing::warn!` after `validate`,
non-fatal. Tests pin both directions (loopback/RFC1918 incl. `172.27.176.1`/
link-local/ollama/`` `:11434` `` local; rejects `api.openai.com`, `example.com`,
`203.0.113.5`, `8.8.8.8`, `172.32.0.1` outside `/12`) and `egress_warnings`
(loopback → none, public `embed.url` → one naming field+host, both public →
two, empty `reason.url` inherits without double-count). 909 passed (+4), 1
pre-existing env flake. Decided by verify-before-claiming (negative control:
flip the final non-IP fallback `false`→`true` and both `egress_warnings` tests
go red), name-the-tradeoff (warn, do not gate — the operator configured it;
the warning makes egress legible, not forbidden), fix-the-root (one detector,
one pure method, one emit site). See the 2026-07-22 CHANGELOG entry.

**Still open:** egress redaction, an allowlist, and an egress log — the item
names them; this slice closed only the warning half.

~~"The full text of everything kern captures transits that provider"
(`concepts/security.mdx:81-96`) when a non-local endpoint is configured — no
redaction, no allowlist, no warning at config load, no egress log. For a project
whose first claim is "local-first, zero egress", the one setting that voids it is
unremarked.~~

### 79. `validate_fact_source` is dead code — closed 2026-07-22 `[surface]`

Called **once** (corrected 2026-07-21 — it was twice; the second site left with
the ingest `kind` arg in `216730d`), with the literal `AGENT_SOURCE`
(`src/mcp_tools_mutate.rs:161`), and it accepts `USER_SOURCE` / `AGENT_SOURCE`
(`src/validate.rs:21`), so it can never fail. Decision: delete. The
"thread a real auth identity" alternative died with the item 18 removal
decision (2026-07-22) — kern carries no caller identity, so this guard can
never become real. Deletion is a small standalone slice, still owed.

**Closed 2026-07-22 by deletion.** `validate_fact_source`, the
`FactFromUntrustedSource` variant, the constants import, and the two dead tests
are gone. Called once with `AGENT_SOURCE` (always `Ok`), so the `Err` path was
unreachable; `clamp_confidence` already caps agent confidence at
`MAX_AI_CONFIDENCE`, so the trust boundary holds and runtime behavior is
unchanged. `cargo test --workspace` green (895 passed), zero grep hits.
Decided by: name-the-tradeoff (delete the unenforceable guard; a future real
principal would need a new check, but item 18 retired that path).

### 81. The proxy serves every method its handshake advertises — closed 2026-07-22 `[surface]`

**Closed 2026-07-22.** `ProxyServer` now implements `handle_method`
(`src/commands_mcp_cmd.rs:368`), so the five methods `extra_capabilities`
(`:355`) advertises — `resources/list`, `resources/read`, `prompts/list`,
`prompts/get`, `ping` — answer instead of falling through the trait default
(`src/transport/src/mcp.rs:137`) to `-32601`. `ping` is the one that mattered
most: clients use it for liveness, so `-32601` there read as a dead server on
the path an agent actually gets (`cmd_mcp` reaches `run_proxy` whenever a
daemon exists).

Four of the five — `resources/list`, `prompts/list`, `prompts/get`, `ping` —
answer from `handle_graphless_method` (`src/mcp.rs:249`), the same function the
standalone `Server::handle_method` (`:219`) dispatches through, so the two
surfaces are one implementation, not two that agree. `resources/read` is the
one advertised method that needs the graph, which the proxy process does not
hold; it rides the existing `call_tool` passthrough as `RESOURCE_READ_TOOL`
(`src/mcp.rs:243`) — a synthetic name encoded by `encode_resource_read` (`:277`)
and decoded by `decode_resource_read` (`:288`), which carries the verdict's
exact code in the text block because `CallToolRes` carries only `content` and
`isError` and an error code that does not survive the hop would turn `unknown
resource` into a generic `-32000`. It is transport, not a tool schema entry:
`tools/list` is unchanged (`ProxyServer::tools_list`, `src/commands_mcp_cmd.rs:301`), so an agent sees
no second name on the surface.

Proved by five Rust tests (`src/commands_mcp_cmd.rs:743`, `mod proxy_method_tests`)
driving the production `serve_rw` loop over a real `ProxyServer` bound to a real
daemon on a scratch socket: every advertised method answers with a non-error
result and no frame carries `-32601`; `resources/read` returns the daemon's own
entity text, proving the round-trip crossed the socket; an unknown URI keeps its
`ERR_NOT_FOUND` code across the `call_tool` hop; the `resource_read` carrier is
dispatchable but absent from `tools/list`; and the four graphless methods answer
identically whether called through the shared function or the standalone server.
Decided by: fix-the-root (one shared dispatcher, not a second copy that can
drift), reuse (ride `call_tool` rather than a fifth `KernRpc` wire method).
Supersedes: nothing — the item's own text described the defect, now removed.

### 82. Standalone `kern mcp` runs no gossip `[surface]` — closed 2026-07-22

**Corrected:** the previous version said "no maintenance tick and no gossip". The
tick *is* started (`src/commands_mcp_cmd.rs:498-509`); only gossip is absent
(`broadcast_q: None` at `:504`, `broadcast_pulse: None` at `:518`). A graph
served that way decays, clusters and GCs normally, and simply does not federate.

**Closed 2026-07-22 — decided, not built.** Standalone `kern mcp` is
intentionally non-federated. It is the lightweight single-project local MCP
server (the fallback a client gets when no daemon is serving, and the path a
developer reaches for to inspect one graph); federation is the daemon's job —
`kern --daemon` runs `start_gossip` (`src/commands.rs`), the maintenance tick
wires `broadcast_pulse` into it, and `do_resolve` raises `broadcast_q`. Spawning
gossip from standalone too would make a process that binds no daemon socket,
holds the writer lock as `mcp-standalone` (`claim_standalone`,
`src/commands_mcp_cmd.rs`), and yet reached peers on its own — a second
federating surface beside the daemon with no supervision, racing the hub a
`kern mcp` auto-spawns. The split is the contract: one federator per host, the
daemon; standalone serves a graph, decays, clusters and GCs, and stops there.
A user who wants federation runs `kern --daemon` (or lets `kern mcp` auto-spawn
one and proxy). Tradeoff taken: a standalone-only deployment does not federate,
which is the point — federation is opt-in and coordinator-free, and standalone
is the opt-out. Decided by: name-the-tradeoff, one-dispatch-core (the daemon is
the one federator; standalone shares the dispatch core but not the federation
leg).

### 83. Nothing bounds memory deterministically: eviction and spill are both disarmed `[lifecycle]`

**Retitled 2026-07-21 — the old title named a knob that does not exist.**
`KERN_CAP_DISABLED` (`src/base_constants.rs:30`) is a *kern-eviction* sentinel,
not a per-kern entity cap; its own comment says so. It defaults both `max_kerns`
and `disk_threshold` to `usize::MAX` (`src/config_graph.rs:18,20`), and those are
the two things it disarms: `enforce_kern_cap` (`src/graph.rs:216`) never
unloads a kern, and the DiskANN spill branch (`src/graph.rs:296`) never
fires. A per-kern *entity* cap for local kerns does not exist at all — the only
one in the tree is `GOSSIP_REMOTE_KERN_ENTITY_CAP` for `remote-*`. Wanted,
unchanged: a safe cap plus an escalation policy. The comment's "currently unsafe"
is a real reason nothing was set — eviction drops unpersisted `children` pushes
(`src/config_graph.rs:16-17`).

**Resident-cap half closed 2026-07-22; DiskANN-spill half stays for item 75.**
The "currently unsafe" comment was **stale, verified**: `get_mut` auto-loads
from the store, so when `register` inside `spawn_unnamed_child` evicts the
parent before its `children` list gains the new id, the post-register
`get_mut(parent)` reloads the persisted copy and the children-push lands on a
reloaded row that persists — no re-spawn loop, no fragmentation. A new test
(`spawn_unnamed_child_under_cap_keeps_the_child_in_parent_children`,
`src/accept.rs`) pins it under `max_kerns = 2` with a store bound.
`GraphConfig::default().max_kerns` is now **128** (was `KERN_CAP_DISABLED`): a
conservative resident bound — most projects carry <10 kerns, 128 bounds the
pathological case, and eviction unloads to the cold tier (it never forgets).
`disk_threshold` stays `KERN_CAP_DISABLED` until item 75 (DiskANN crash
consistency) closes — arming it would expose the spill path's crash window, so
the spill half of "bounds memory" remains open there. A per-kern *entity* cap
for local kerns still does not exist; the resident-kern bound is the half that
satisfies "the hot graph stays bounded" without touching DiskANN. An explicit
`max_kerns = usize::MAX` in a `kern.toml` still opts out (the sentinel is the
uncapped marker).

**The double-storage half shipped 2026-07-21; the bounding half is untouched.**
Item 29 finding 3 assigned it here: every vector was resident twice. Verified
before building rather than assumed — `index_kern_into` handed the index
`t.vector.clone()` (`src/graph.rs:34`, `gnn_vector` `:38`, reasons `:46`)
and `HnswNode` stored that clone verbatim, because the shipped default is
`QuantizationMode::None` (`src/quant.rs:8-9`; int8 is opt-in through
`kern compress`, and under it the node's float vector was already empty, so this
buys nothing there). The two copies were the same floats, not a normalised one
and a raw one — recall is unmoved to four decimals across the change, which is
the check that would have caught it had they differed. `Entity::vector`,
`Entity::gnn_vector` and `Reason::vector` are now `Embedding`
(`src/base_types.rs:592`) and every index holds the map's own allocation.

Measured with `tests/spill_memory.rs` in `resident` mode — 50k entities at dim
384 each carrying `vector` AND `gnn_vector`, plus 25k reasons, one process per
reading, ten interleaved before/after pairs in release:

| | hot RSS | index walk, median |
| --- | --- | --- |
| before | 510.2 MB | 190 µs |
| after | **324.6 MB** | 211 µs |

**−185.6 MB, −36.4%**, with 0.5 MB of spread across ten runs. The three
`*_only` rows are the control — they still copy, and the type now makes that
copy visible as a `to_vec` (`src/graph.rs:336` does the same for the
DiskANN build input) — and they moved ≤1.3 MB, which is the `Arc` header on
125k allocations.

The cost is query latency, and naming it is the point: **+17 µs median on a
~190 µs index walk (+9%), slower in 11 of 15 paired runs.** It is not the atomic
refcount, which is never touched on the read path, and not the indirection —
`Arc<[f32]>` derefs exactly as `Vec<f32>` does. The likely mechanism is
locality: the index's vectors used to be allocated together during its build and
now point into the scattered kern map. That mechanism is unproven, and the
measurement cannot settle it — the host carried load average 4-6 from two
sibling cycles throughout and the before-arm spread was ±60 µs, the same order
as the effect. What is safe to claim is that it is not a step change.

Still open here, and the reason this item does not close: **nothing bounds the
resident set.** Halving the O(N) term moves the ceiling; it does not install
one. Also deliberately unclaimed, so that the number above measures one change:
`do_reembed` seeds `vector` and `gnn_vector` from the same embed
(`src/tick_tasks.rs:541-542`) and they could share a third allocation until GNN
propagation overwrites one — another 76.8 MB at this corpus size.

**Reembed double-alloc half closed 2026-07-22.** At reembed `vector` and
`gnn_vector` now share the `Arc` — `e.vector = v.clone().into();
e.gnn_vector = e.vector.clone();` (`src/tick_tasks.rs`, `src/commands_reembed.rs`)
— one alloc + one `Arc::clone` (refcount bump, zero alloc) instead of two
`Arc::from(Vec)`, saving ~76.8 MB at 50k/dim384. No COW, no behavior change: the
shared `Arc` drops one refcount the moment GNN propagation replaces `gnn_vector`
(`gnn_propagate.rs` Arc-swap, never in-place), and no path mutates either in
place. Proved by `do_reembed_shares_vector_allocation_with_gnn_vector`
(`Arc::ptr_eq(&e.vector, &e.gnn_vector)` after `do_reembed`); negative control
(revert to `v.clone().into()` → not ptr-equal) reds, green on revert. Existing
`do_reembed_*` and `a_completed_reembed_restamps_the_store_with_the_new_model`
green unedited. `cargo test -p kern --lib` 945 passed, 0 failed, 4 ignored.
Decided by fix-the-root (share the `Arc` the values already are, not a COW
layer), name-the-tradeoff (the saving is per-reembed, not steady-state — GNN
propagation breaks the share after the first tick), verify-before-claiming
(negative control). See the 2026-07-22 CHANGELOG entry.

**Signal-on-approach half closed 2026-07-22.** The armed `max_loaded_kerns` was
unreported; now surfaced. `GraphGnn::max_loaded_kerns()` (new pub accessor) +
`HealthStats.max_kerns` (new field, filled in `graph_health_stats`) +
`kern health` prints `kerns: N (cap M)` (or `kerns: N (cap off)` when
`M == KERN_CAP_DISABLED`) and warns `kerns near cap: N/M` at
`KERN_CAP_APPROACH_FRAC` (new, `src/base_constants.rs`, default `0.9`),
**daemon-sourced only** (item 100 rule — the CLI's fresh-open graph is
structurally small). MCP `health` JSON carries `max_kerns`; `trnsprt::HealthRes`
gains `#[serde(default)] max_kerns` (same shape as `gini_access`, item 62) so an
old daemon reads `0` → `cap off`. Proved by
`graph_health_stats_reports_max_kerns` (default → `KERN_CAP_DISABLED`; `set(8)`
→ `8`), `kern_health_warns_when_resident_kerns_approach_cap` (116/128 → warn,
10/128 → none, `u64::MAX`/`0`/no-daemon → none), dto round-trip `max_kerns:
128`; existing kern-health + `HealthStats` tests green unedited. `cargo test -p
kern --lib` 950 passed, 0 failed, 4 ignored; `cargo test -p trnsprt --lib` 61
passed. Negative control (force the approach check `false` → 116/128 yields no
warn → test reds, green on revert). Decided by fix-the-root (surface the armed
cap, do not re-tune it), name-the-tradeoff (daemon-sourced only; `disk_threshold`
stays silent — spill disarmed until item 75), verify-before-claiming (negative
control). See the 2026-07-22 CHANGELOG entry.

**Per-kern entity-count signal added 2026-07-23.** `HealthStats.largest_kern_entities`
(new field, max `Kern::entities.len()` across resident kerns, filled in
`graph_health_stats`'s existing walk) — a gauge of the unbounded resident set at
the granularity the kern-cap (bounds *count* of kerns, not *size* of any one)
cannot reach. A 128-kern graph with one kern holding 100k entities is "within
cap" and was invisible; now `kern health` prints `kerns: N (cap M, largest L
entities)` (or `cap off, largest L entities`), **daemon-sourced only** (item
100 rule). MCP `health` JSON carries `largest_kern_entities`; `HealthRes` gains
`#[serde(default)]` (old daemon → `0`, same shape as `gini_access`/`max_kerns`).
Proved by `graph_health_stats_reports_largest_kern_entities` (empty → `0`; one
kern holding 10 + four empty → `10`), dto round-trip `99`. `cargo test -p kern
--lib` 954 passed, 0 failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed.
Negative control (`largest_kern_entities = 0` skip the max → 10-entity fixture
yields `0`) reds, green on revert. Decided by fix-the-root (measure, do not
enforce — the per-kern cap is blocked on the absent cold-spill, item 75),
name-the-tradeoff (one max, not a distribution; the smallest signal naming the
gap), verify-before-claiming (negative control). See the 2026-07-23 CHANGELOG
entry.

**Gini-over-kern-sizes gauge added 2026-07-23.** `gini_over_kern_sizes(counts:
&[usize]) -> f64` (new pure fn, `src/health.rs` beside `gini_over_access`)
— the distribution the `largest_kern_entities` max only summarises: `0.0` =
balanced (all kerns equal), → `1.0` asymptotically (**finite-n max `(n−1)/n`**,
stated not hidden, same as item 62). `HealthStats.gini_kern_sizes` (new field,
filled from the same resident-kern walk). `kern health` `kerns:` line gains
`gini N.NN`; MCP `health` JSON carries `gini_kern_sizes`; `HealthRes` gains
`#[serde(default)]` (old daemon → `0.0`), **daemon-sourced only** (item 100).
Proved by `gini_over_kern_sizes_pins_known_distributions` (`[10,0,0]` → `2/3`,
`[100,0]` → `1/2`), `graph_health_stats_reports_gini_kern_sizes` (10 + four
empty → `4/5` > `0.5`; empty → `0.0`), dto round-trip `0.42` + old-payload →
`0.0`. `cargo test -p kern --lib` 961 passed (1 pre-existing `the_sink_waits`
flake, green isolated); `cargo test -p trnsprt --lib` 61 passed. Negative
control (`gini_over_kern_sizes → 0.0` always reds, green on revert). Decided by
fix-the-root (measure the distribution, do not enforce — per-kern cap blocked
on item 75), name-the-tradeoff (a distribution, not a second max; finite-n Gini
maxes at `(n−1)/n`, stated), verify-before-claiming (negative control). See the
2026-07-23 CHANGELOG entry.

### 84. Remaining operational odds and ends `[surface]`

- **`serve.mcp_addr` is a config field with no reader.** ~~Added when item 11 landed~~ **Closed 2026-07-22.** `run_server` now resolves CLI flag first, falls back to `cfg.serve.mcp_addr`.
- **`kern merge` still defaults on a broken foreign config.** ~~`src/commands_admin.rs` does `Config::load(..).unwrap_or_else(..)` for both the source and destination roots~~ **Closed 2026-07-22.** Both src and dst configs now fail loud on load error.
- ~~**`num_ctx` / `keep_alive` / `num_gpu` cannot warn when ignored on the `/v1`
  path**~~ — **Closed 2026-07-22.** `num_ctx` and `keep_alive` are now real
  per-endpoint config keys on `[embed]` and `[reason]` (defaults = the former
  constants in `src/llm.rs`, now `pub`), threaded through `Client` via
  `with_embed_num_ctx` / `with_embed_keep_alive` / `with_reason_keep_alive`
  (and `with_num_ctx` for reason, now 0-keeps-default). `wants_native` is
  exposed as `pub fn is_openai_compat` (`src/llm.rs`), and
  `Config::native_knob_warnings` (`src/config.rs`) emits one non-fatal
  `tracing::warn!` at boot per knob a config sets on a `/v1` endpoint — default
  knobs on `/v1` are silent, a default is not "trying to tune". `num_gpu` was
  never a knob kern sends, so it is not a config key; nothing ignores it.
- ~~Hand-rolled tool schemas; no batch query~~ — **the batch-query half
  closed 2026-07-22.** The `query` MCP tool now takes an `ids` array for batch
  direct lookup, returning `{results, missing}` so a caller can tell a
  filter-drop (in `results`, flagged) from a non-existent id (in `missing`).
  Each id resolves a prefix and the cold tier and honours the same filters the
  single-`id` path honours — the per-row predicate, not a silent skip. The
  ~~hand-rolled-schemas half stays~~ — **retired 2026-07-22 as accepted
  style debt.** The tool schemas are hand-written JSON (`tools.rs`), correct,
  unit-tested (`definitions_are_well_formed_and_complete`), and stable. Deriving
  them from types would need a proc-macro for a refactor with no correctness,
  ergonomics, or wire gain — the schemas ARE the contract an MCP client reads.
  Kept hand-written by decision; a future derive lands only if the schema surface
  grows enough that hand-maintenance costs more than the macro.
- ~~The LLM client is Ollama-centric with no retry/backoff policy object~~
  — **retry/backoff half closed 2026-07-22.** `complete` now retries a transient
  (5xx/429/timeout/connect) with the embed leg's `[150, 300, 600]ms` cadence via
  a new `post_with_retry` (`src/llm.rs`) before surfacing the failure — a
  gateway blip no longer re-queues a whole distill transcript. `complete_func`
  still records the *final* failure once (a recovered completion is not a
  failure). The `Ollama-centric` half stays: the client speaks Ollama-native
  - OpenAI-compat and no other provider, by design (local-first). New test
  `complete_retries_a_transient_5xx_then_succeeds`. 1038 pass.
- ~~Watcher `.gitignore` parsing is approximate; no rename tracking~~ **(retired
  2026-07-21 — verified false on both counts).** `IgnoreRules` builds a real
  `Gitignore` through ripgrep's `ignore` crate
  (`src/watcher_ignore_rules.rs:3`), i.e. the full spec, and
  `WatchKind::Renamed {from, to}` carries both endpoints
  (`src/watcher_event.rs:9`). What survives is narrower and still open:
  **a rename is not re-keyed in the graph.** `build_record`
  (`src/watcher_pipeline.rs:48`) ingests `to` and discards `from`, so the
  renamed file lands as a new `Document` and the old one is neither moved nor
  removed. It duplicates only when the rename *also* edits the file — ids are
  `content_hash(text)`, so an untouched move re-resolves to the same id, while
  `external_id` is the path (`src/ingest_file_watcher.rs:185`), so a
  move-plus-edit gets a new id under a new external id and supersede never
  fires. ~~It sits in this tier and not in tier 1 because the watcher is **off by
  default** — `WatcherConfig::enabled` is `false` unless a `kern.toml` sets it
  (`src/config_watcher.rs:14-16`) — so it is not a default-path defect
  (`FEATURES.md:1085-1088`).~~ **Closed 2026-07-22 (both halves).** The
  rename+edit half (`old_id != new_id`) closed in `789968a` — `supersede_renamed`
  supersedes the old-path `Document`. The **pure-rename half** (`old_id ==
  new_id`, content unchanged) closed now: `supersede_renamed`
  (`src/accept.rs:578`) gains `new_external_id: &str` and on `old_id ==
  new_id` re-keys the survivor — `entity.external_id = new_external_id`,
  `clear_source_entry(old)`, `set_source_entry(new)`, then `return None` (no
  supersede edge — same entity). The `file_watcher.rs` caller passes
  `source.object_id()`. Proved by `a_pure_rename_re_keys_the_survivor_external_id`
  (survivor `external_id` == new path, old source-index cleared, new set,
  survivor active); negative control (revert to bare `return None` → `external_id`
  stays old path) reds, green on revert.
  `a_rename_plus_edit_supersedes_the_old_path_document` and
  `a_rename_with_no_old_entity_is_a_noop` green unedited. `cargo test -p kern
  --lib` 937 passed, 0 failed, 4 ignored. Decided by fix-the-root (re-key the
  survivor, do not mint a new entity on a pure move), name-the-tradeoff
  (rename stays in the same kern — a cross-kern move is `move_entity` / item 60,
  not this), verify-before-claiming (negative control). See the 2026-07-22
  CHANGELOG entry.
- ~~`unnamed` lists only; there is no `promote`~~ — **Closed 2026-07-22.**
  `kern unnamed promote <id> <name> <seed> [--mass N]` gives an existing unnamed
  kern a graviton in place: no move, no id change, no re-register — the kern
  keeps its entities, children and parent, and becomes `is_named` so gc keeps it
  (a transient spill child pinned into permanence). `accept::promote_unnamed`
  (`src/accept.rs`) sets `graviton_text`/`graviton_vec`/`mass` on the
  existing `Kern`, embedding the seed via `seed_examples`+`mean_pool` the way
  `graviton add` does. The CLI is async + local (`with_graph`, guarded flush —
  the `kern graviton` route's no-daemon fallback shape), and resolves the short
  id `kern unnamed` prints. Two new tests pin promote + reject-named/missing.
- GNN has no GPU path, weights are per-kern rather than shared, and the objective
  is link-prediction only (`FEATURES.md:595-596`).
- ~~Under WSL2 NAT a loopback Ollama URL must be hand-pinned; kern neither
  rewrites nor warns~~ — **Closed 2026-07-22.** kern still does not rewrite
  config (a URL is the user's to set), but it now warns at boot when it is
  running under WSL and a configured `embed.url` / `reason.url` is loopback
  (`127.0.0.0/8`, `::1`, `localhost`) — a Linux loopback does not reach a
  Windows-host Ollama, and the guest needs the RFC1918 gateway IP.
  `crate::llm::is_wsl` reads the Microsoft marker in
  `/proc/sys/kernel/osrelease`; `crate::llm::is_loopback_url` is the
  loopback-only subset of `is_local_url` (a WSL2 gateway `172.27.x.x` is local
  but not loopback, so it stays silent); `Config::wsl_loopback_warnings`
  emits one non-fatal `tracing::warn!` per loopback endpoint, alongside the
  `egress_warnings` (item 78) and `native_knob_warnings`. Non-WSL hosts are
  silent — loopback is correct there (`FEATURES.md:1125-1127`).
- RPC socket bind→chmod race — sub-millisecond, umask default — recorded as an
  accepted risk where it happens (`harden_socket`,
  `src/transport/src/typed/local.rs:348-358`); revisit only if the umask
  alternative stops being worse. **Corrected 2026-07-22:** this cited
  `concepts/security.mdx:40-43`, which is the API-key-vs-redirected-endpoint
  rule and says nothing about the socket; that page states the `0600` mode at
  `concepts/security.mdx:16` and does not record the race at all.

### 101. Every anchor the widened checker exposed now points where its sentence says — closed 2026-07-22 `[process]`

`docs-check` learned to resolve bare continuation refs — `` `:239` `` following a
`` `place.rs:112` `` — and immediately nominated a list that was always wrong and
always invisible. The count is a measurement of the old blind spot, not of new
damage: nothing broke, the checker started looking. The merge that filed this
counted 18; the list read off this branch is **20**. The discrepancy was not
chased — the list is what the checker prints, not what the filing remembered.

Each of the 20 was adjudicated by reading the citing sentence, opening the cited
line, finding where the described thing actually lives, and reading the new
target back. The list resolved into four classes, not the two the filing guessed:

- **Stale line numbers**, the ordinary rot — 11 of them. The `move` row's
  `` `tools_mutate.rs:491` `` was three lines short of `tool_move`; the `Acl`
  struct had moved 13 lines; `mcp_cmd.rs`'s two `None`s had drifted 18.
  Mechanical, one target read at a time.
- **A bare ref continuing the wrong file** — 4, plus a fifth wearing a
  quotation's clothes. Item 52 documented the hazard and it is exactly as
  predicted: `bind_unix`'s `` `:507` `` bound to `src/commands.rs` and meant
  `src/transport/src/typed/local.rs`; `Worker::submit`'s `` `:129` `` bound to
  `src/ingest_direct.rs` and meant `src/ingest_file_watcher.rs` — the *number*
  was right, only the file was wrong, which is the quietest failure of the set;
  and item 75's two doc-only leads bound to `src/graph.rs` while meaning
  `docs/diskann-disk-index.md`. None of these can be fixed by adjusting a
  number. Each citation is now spelled in full at the point the file changes,
  and only the continuations that follow it are left bare.
- **A quotation the checker read as a citation** — 2, both in this item's own
  first draft, which named the broken anchors in single backticks and so made
  fresh copies of them. The fifth counted above — item 30's note on the dead
  `concepts/acceptance.mdx` citation — is the same defect wearing a wrong-file
  binding as well. The repo already has the fix: a doubly-backticked span is an
  illustration and is blanked before scanning. The lesson is narrow and worth
  keeping: **a page describing a broken citation must display it, not make it.**
  Writing this entry reproduced the fault twice before the checker caught it.
- **False positives** — 2, and both are the same shape. The `gc` row cited
  `tool_gc` correctly, and `README.md:399` pins the version correctly; in both <!-- docs-check: anchor-ok -->
  the only distinguishing token is under the three-character floor (`gc`) or is
  digits the tokeniser discards (`1.1.0`). Neither was acquitted with
  `anchor-ok`. The `gc` anchor was widened by two lines to reach the `reaped` /
  `before` / `after` binding the row actually describes, and the `README` line
  now names the anti-entropy pointer that shares it — both true of the target,
  and both leave the anchor checked rather than silenced.

**Two out-of-band repoints came with them**, found by reading targets rather than
by nomination: `Entity::acl` was cited into `src/base_types.rs` at `` `:296` ``
(an unrelated field) and `start_gossip`'s builders into `src/commands.rs` at
`` `:1046-1127` `` (an unrelated span). Both are wrong the same way and neither
was nominated, because a long enough citing block shares words with almost
anything. That is the residual: the content check bites where the sentence is
short.

11 + 5 + 2 + 2 = 20. So the false-positive rate on the widened checker's first
real list is 2 in 20, and both are floor artefacts rather than judgement errors
— which is the number `--strict-anchors` has to be adopted against.

Note the shape of the win: the checker's own improvement is what made its
previous coverage claim false. Every "no anchors nominated" before this widening
meant "none among the 63% I could parse".

### 99. The watcher's off-limits set is a list of names, not an invariant `[ingest]`

Item 30's durable backstop put a kern-written file inside the default watched
root and the watcher ate it — 283 payloads from one seed edit before the fix. The
fix works and is measured: `IgnoreRules::with_denied`
(`src/watcher_ignore_rules.rs:45`) takes the resolved `intake.dir` and
`data_dir` from `spawn_file_watcher` (`src/commands.rs:1034`). But it closes that
loop by *enumerating* the two directories that were writing, not by making the
class impossible. Anything kern writes under a watched root in future is
ingestible again unless someone remembers to add it, and there is no test that
fails when they forget — the regression e2e
(`tests/e2e/test_file_watcher_durability.py`) pins the two dirs that exist today.

Two shapes worth weighing, and neither is obviously right. **One state root**:
declare that everything kern writes lives under a single prefix and deny that
prefix, which is nearly true already (`.kern/` holds config, intake and data) but
false the moment `data_dir` is pointed outside it — which is a supported config
and the reason the deny list is computed rather than hardcoded. **Deny at the
writer**: have the code that opens a path for writing register it, so the deny
list is derived from what the process actually holds rather than from a list
maintained by hand. The second is correct and costs a registry the daemon does
not have.

Ranks here and not in tier 1: the two paths that matter today *are* denied, the
failure mode is amplification rather than a wrong answer, and the whole watcher
is off unless a `kern.toml` enables it (`src/config_watcher.rs:8`). <!-- docs-check: anchor-ok -->
It is a latent correctness hole, not a live one.

**Closed 2026-07-22 — the "one state root" half shipped, and it is the invariant
for every writer today.** `spawn_file_watcher` now denies `.kern/` whole (via a
new pure `watcher_denied_paths(cfg, cwd)` in `src/commands.rs`), in addition to
the resolved `intake.dir` and `data_dir`. The union is the invariant item 99
names: everything kern writes is denied, whether it lives under `.kern/`
(config, logs, mcp-token, default data + intake) or under a `data_dir` /
`intake.dir` pointed outside `.kern/` (a supported config — and the reason the
deny list is computed, not hardcoded). Denying `.kern/` whole closes the hole
the two-dir enumeration left: a future writer under `.kern/` is covered without
remembering to add it. What stays open, and is the registry the item's second
shape names: a future writer under a watched root but outside *both* `.kern/`
and the configured data/intake dirs would still need a line here — none exists
today, and the union is the derived invariant for every writer that does. The
regression e2e still pins the two dirs; two new unit tests pin
`watcher_denied_paths` (default + data-dir-outside-`.kern`). 1023 pass.

Deciding behavior: fix-the-root — the enumeration is the symptom-level fix, kept
because the root fix is a registry and the loop was live; this half closes the
state-root invariant the registry was the full answer to.

### 87. Do the preset tiers earn their numbers, now that `relaxed` is the default? — measurement half-closed 2026-07-23 `[eval]`

**Measurement half-closed 2026-07-23; tuning sweep still open.** The active
preset name (`relaxed`/`medium`/`tight`) is now surfaced: `Server::health_stats`
(`src/mcp.rs`) JSON carries `preset` from `self.cfg.preset`; `trnsprt::HealthRes`
gains `preset: String` `#[serde(default)]` (old daemon → `""`); `kern health`
prints `preset: {name}` **daemon-sourced only** (item 100 rule), placed first
so it frames the heat/recency/retrieval lines; `kern://local/health` by
construction. Proved by `kern_health_prints_preset` (tight/relaxed/empty/no-
daemon) + dto round-trip `preset: "tight"`; standing negative-control guard:
old-payload absence decodes `""` (the `tight` print reds if the field is
omitted). `cargo test -p kern --lib` 963 passed, 0 failed, 4 ignored; `cargo
test -p trnsprt --lib` 61 passed. Decided by fix-the-root (surface the name
that frames the values, do not tune — tuning is the sweep below), name-the-
tradeoff (one name — the values already surface via heat/recency/retrieval),
verify-before-claiming (old-payload-absence guard). See the 2026-07-23
CHANGELOG entry.

**Still open:** the tuning sweep — run the retrieval suite once per preset,
re-pin the baseline to `medium` or re-record on `relaxed`, adjust any knob
whose tier value scores worse than medium on the posture it claims to serve.

~~**Deciding behavior: verify-before-claiming.**

The preset tiers shipped 2026-07-21 with hand-picked values, and the default
flipped from the medium-era values to `relaxed` the same day
(`src/config_preset.rs`). Every prior measurement — including the LoCoMo
0.137 baseline — ran on medium-era defaults, and a configless run now
exercises `relaxed`. Question: run the retrieval suite once per preset;
first decide whether the standing baseline is re-pinned to `preset =
"medium"` or re-recorded on `relaxed`, then adjust any knob whose tier value
scores worse than medium on the posture it claims to serve (relaxed →
recall, tight → precision). Preset values live in code now, so adjustments
are commits, not config edits. Blocked on nothing; gated only on an idle GPU
and item 1's instrument staying the scorer.~~

### 85. Documentation owed, tracked here so it is not lost `[docs]`

- (retired 2026-07-22 — `docs/site/content/docs/howto/hub.mdx` now documents it,
  including the 1800s idle-unload default, the `[hub]` `auto_start` / `auto_restart`
  keys, and `merge` / `stop` / `unload` / `resolve`.) `kern hub` appears on no
  site page, including its 1800s idle-unload default — a user whose daemon vanishes
  had nothing to read. (~Verified absent before the fix: `grep -rl "kern hub"
  docs/site/content/docs/` returned nothing.~)
- (retired 2026-07-21 — the id path stopped bypassing filters and the page was
  corrected in the same change) the `id` path in `query` bypassed every filter
  (item 18); `howto/mcp.mdx:50` now says every filter applies to an `id` read.
- (retired 2026-07-21 — the tables were filled in) the `move` MCP tool is listed
  in `README.md:352` and `FEATURES.md:621`, and the site's count is now thirteen
  (`howto/mcp.mdx:5, :75`), so the "Eleven tools" note is retired with it.
  <!-- docs-check: anchor-ok -->
- (retired 2026-07-22 — the declaration is corrected to the truth rather than
  the five notes rewritten: `docs/kern/README.md` now states the directory holds
  reference records and that any forward-looking plans a note carries are
  historical, with `ROADMAP.md` alone authoritative for new work. Law 4's intent
  — ROADMAP is the only plan — is kept; the lie that the notes held no plans is
  gone.)
- (retired 2026-07-22 — `README.md:116` now names the typed `KernRpc` as
  generated by this repo's own `service!` macro in `src/transport/`, matching
  `FEATURES.md` §13. The phantom `tarpc` dependency is gone from where a reader
  looks first.) ~~`README.md:116` still sells "**tarpc `KernRpc`**" on the front
  page. There is no tarpc in `Cargo.toml`, in `Cargo.lock` or anywhere in
  `src/`~~ — the service is generated by this repo's own `service!` macro
  (`src/transport/macros/`), and `FEATURES.md` §13 says so outright. (found
  2026-07-21, same sweep that corrected the identical line in `SPECIALISTS.md`.)
- (retired 2026-07-21 — corrected at the source) the four stale `docs/kern/`
  research-note claims are gone: `crdts-federation.md:6-14` now states outright
  that no `PnCounter` was ever built, that `Delta` has a live sender, and that
  OR-Set for `statements` was *reversed, not deferred*;
  `diskann-disk-index.md:28-29` marks the PQ recall claim **withdrawn**.
- (retired 2026-07-21 — withdrawn in place) the quality claims item 1's standard
  forbids no longer survive. `FEATURES.md:195` keeps only the `+7% p50` latency
  half and says the retrieval-quality half is withdrawn;
  `docs/diskann-disk-index.md:26` says the note "previously published" the
  `recall@10 ≥ 0.90` figure; this file's own "recall@10 A/B" citation was struck
  earlier.
- (retired 2026-07-21 — `README.md` and `VISION.md` were corrected) neither
  opens on "takes in durable facts from your sessions" or "learns on its own"
  any more; `VISION.md:52` now *states* there is no recorded baseline instead of
  gating claims on one; `README.md:402-403` says the Question and Pulse senders and
  the fetch RPC are live, and `:407` pins the version at `1.1.0` beside the
  anti-entropy pointer, matching `FEATURES.md`.
- (retired 2026-07-21 — all three fixed) `FEATURES.md:54` now lists `Entity`'s
  `acl` <!-- docs-check: anchor-ok -->; the retired query-cache finding is gone from the file entirely; and
  `:634-635` marks prompts and resources "served on the standalone path only",
  which is item 81's note.

Deferred design calls, still owed, no blocker and no urgency: quarantine
representation (bool vs `EntityStatus::Quarantined` vs `Source` trust band);
contradiction-reconcile gating band; temporal-aware as-of retrieval scoring;
episodic abstraction as a tick task; chunking strategy (contextual-prepend vs
proposition self-containment). The threat model and staged hardening list
(ed25519 signing, peer trust, Sybil binding, replay protection)
lived in `docs/kern/safety-architecture.md`, deleted 2026-07-20 for stale paths —
recover from git history when tier 5 opens. A private-disclosure policy still
exists at `docs/FEDERATION-SECURITY.md:24-28` with nothing behind it.

---

# Context — not work

## North star

kern is the memory layer an agent recalls from: local-first, in-process,
per-cwd, offline-capable, self-forgetting, with no query-time LLM on the default
path — and it **retrieves the right thing**, provably.

**There is no recorded baseline.** The LoCoMo eval, the retrieval bench, and
`docs/kern/locomo-baseline-2026-07-19.json` were all deleted in `8d8b19e`
(2026-07-20). That deletion was correct and is not to be undone as-was: the
LoCoMo score collapsed ingest × retrieval × answering into one LLM-judged number
in which the **answering term dominated**. Measured the same day, a grounded run
— whole conversation in the prompt, kern bypassed entirely — scored 0.187 on a
slice where kern scored 0.027. The ceiling was set by a 3B answerer, not by
memory, so the number could not steer memory work. Three eval-side prompt changes
moving one slice from 0.131 to 0.027 in a single day confirmed it was measuring
the harness.

The previously published figures (overall 0.137 ± 0.018, "gap 0.46") are
therefore **withdrawn, not superseded** — no current number replaces them.

Claim standard, amended 2026-07-22 by item 103's first measured run:
**retrieval-only claims are permitted, and only those.** A permitted claim
names the harness (`tests/e2e/eval/`), the dataset, the pinned embedder, and
the hit definition (any-hit vs all-hit) — e.g. "LoCoMo-10 recall_any@5 0.5983,
qwen3-embedding:0.6b, direct path". End-to-end quality claims (anything a
reader could compare to an LLM-judged J-score) remain forbidden: kern has no
number in that protocol family and the family itself does not reproduce.
Latency claims remain permitted from the e2e harness. Item 1's scorer
(`tests/e2e/test_recall.py`) still certifies nothing — it steers work and
catches regressions against a bag-of-words embedder.

## How we supersede Zep / Mem0 / Letta / Qdrant

Not by matching feature lists. By owning a combination none of them hold, then
proving it — on a *comparable* measurement. Item 1 shipped a scorer that
measures kern against its own past, not against them; item 103's harness
(2026-07-22) scores the public datasets retrieval-only, comparable to the one
rival that publishes in the same protocol family (YourMemory) — first real
LoCoMo-10 run recorded the same day at any@5 0.5983 against YourMemory's
vendor-run 0.59. Parity with one unverified peer number, not supersession.

| property | kern | Zep/Graphiti | Mem0 | Letta | Qdrant |
| --- | --- | --- | --- | --- | --- |
| Per-project self-maintaining graph (per-cwd) | ✅ | ❌ hosted | ❌ | ❌ | ❌ |
| Default recall touches no LLM (sub-ms) | ✅ | ❌ | ❌ | ❌ | n/a |
| Local-first, single binary, no network hop | ✅ | ❌ | ❌ | partial | ❌ |
| Self-forgetting (decay / stigmergy GC / cold spill) | 🟡 items 32, 54 | ❌ | partial | ❌ | ❌ |
| Graph + dense ANN + BM25 + GNN in one process | ✅ | partial | ❌ | ❌ | ❌ |
| Bi-temporal supersede off the recall path | ✅ local / 🟡 item 44 federated | ✅ | ❌ | ❌ | ❌ |
| Coordinator-free CRDT federation | 🟡 building | ❌ | ❌ | ❌ | ❌ |
| Published eval numbers | 🟡 LoCoMo-10 recorded 2026-07-22, item 103 | ✅ | ✅ | ✅ | n/a |

Two rows carried an unqualified ✅ in an earlier version while this same file
funded the defects that break them. They are 🟡 with the items named — a
scoreboard that disagrees with its own plan is not a scoreboard. Re-pointed
2026-07-21: the items they originally named (2, 5, 6 and 4, 17) have all closed,
and a qualifier citing a retired number reads as an open defect that no longer
exists. Self-forgetting is qualified by what is still open — tree-depth
eviction bias and the missing GC convergence gate — and supersede is ✅ locally,
qualified only where the stamps stop, at the wire.

**The three moves, in order:**

1. **Get a measurement worth steering by.** The architecture argument is won on
   paper and currently unprovable. We are the only one in this table with no
   published number — honest, and the single biggest gap.
2. **Ship what a hosted service structurally cannot.** Offline, per-cwd, zero
   egress, sub-ms default recall, self-forgetting. These are not features they
   are behind on — they are features their business model forbids. Federation is
   the same bet: no shipped competitor has it.
3. **Refuse the vector-DB fight.** Qdrant parity (PQ, payload indexes, sharding,
   RBAC, SDKs, multitenancy) is a non-goal. Mounting Qdrant as a backend yields a
   superset, not supersession, and forfeits the only structural advantage kern
   has. Repo law forbids a pluggable backend.

Closest rivals per axis: **YourMemory** (decay + published pure-retrieval
LoCoMo/LongMemEval numbers — the one peer in item 103's protocol family; read
before quoting ourselves), **Graphiti** (temporal semantics), **mnemo** /
**AgentDB** (Rust + embedded + MCP stack), **Cognee** (self-hosted KG). The
written survey was deleted with the LoCoMo purge (`8d8b19e`); the 2026-07-22
re-survey lives in the CHANGELOG entry that shipped item 103's harness.

## Non-goals

None of these move an agent-memory eval score. All are table stakes only in a
multi-tenant hosted-DB business kern is not in. Revisit only if that business
materializes.

Distributed sharding (Raft) · replication + write-consistency factor · API key /
JWT-RBAC / TLS-for-clients / audit logging · public REST + gRPC + multi-language
SDKs · multitenancy · GPU index building · product quantization / SPLADE sparse
vectors / ColBERT multi-vector — re-promote any one of these **iff** item 1's
replacement metric shows a retrieval-quality gap it would close.

**At-rest encryption is refused at this layer**, not omitted
(`concepts/security.mdx:61-70`): the store is a local file under the user's own
account, and encrypting it there buys nothing an OS-level FDE does not already
give while adding a key-management surface. Recorded here because a reasoned
refusal that appears in no non-goal list reads as an oversight.

**Parked indefinitely:** the v2 self-training track (LoRA in Rust, teacher
pipeline over mature graph regions, per-graviton adapters hot-swapped at query
time, adapters gossiped by content hash). Nothing built, nothing scheduled. Gate:
an overall eval score that makes specialization worth funding.

## Repo laws

1. **Append-only bincode.** Persisted enums/structs grow by appending only; guard
   schema touches with a round-trip test.
2. **No pluggable/fallback backend.** All-internal, in-process, self-contained.
3. **One dispatch core.** Every surface goes through `mcp::Server::call_tool`,
   never a second copy.
4. **This file is the only plan.** New work goes here, not into a new document.

## Closed and verified — do not re-open

- **The second dedup gate no longer lies about what it did** — was item 91
  `[ingest]` (the third item to carry that number), closed 2026-07-21. Both
  halves shipped in `src/ingest_place.rs`. `place_document` returns
  `Some(result.entity_id)` instead of an unconditional `Some(doc_id)`, so
  `finalize_doc_identity` — which infers dedup from `surviving_id != content_id`
  and was **correct all along, merely fed a lying id** — now reports `Deduped`
  where it reported `Committed`. And both `lex.insert` calls are gated on
  `!result.deduped`. Gating the index is the right half rather than reindexing
  the discarded id under the survivor, because the discarded id names nothing:
  `seed_lexical` (`src/retrieval_seed.rs:96`) does not filter by graph presence,
  so the ghost reached `fuse::rrf`, was rescored to `0.0` by
  `find_entity_ref_in_graph`'s `unwrap_or(0.0)` (`src/retrieval_query.rs:118-120`)
  and *displaced* a live seed. Removing it is a strict win; carrying the wording
  onto the survivor is a separate, larger fix, filed as item 94.
  Proven by reverting each half separately: restoring `Some(doc_id.to_string())`
  fails `a_second_gate_dedup_reports_deduped_and_the_surviving_id` on
  `left: 0302188…` / `right: 64989cc…` and leaves the chunk test green;
  ungating the two `lex.insert`s fails
  `place_chunks_second_gate_keeps_the_discarded_id_out_of_the_lexical_index`
  alone. `place_document` also stopped cloning the whole `Entity` under the
  write guard — `tid`/`joined` are read off it before the lock, mirroring
  `place_chunks`; nothing in that hoist reads guard-held state.
- **Deleting a source cascades into the graph** — was item 19, closed
  2026-07-21. `forget_by_source(scheme, object_id, force)`
  (`src/commands_graph_ops.rs`) resolves every entity whose `Source` matches the
  pair across all resident kerns and cascades through the existing
  `forget_entity`, so edge removal has one implementation. The key is
  deliberately `(scheme, object_id)` and **not** `source_id`, which hashes the
  section too — keying on that would forget one chunk of a document and leave
  the rest. Reachable as the MCP tool `forget_by_source` and as
  `kern forget --source <scheme>://<object_id> [--force]`, which routes through
  the daemon with the local path as the `NoDaemon` fallback (item 9's contract);
  `a_routed_forget_by_source_mutates_the_serving_daemons_graph` and an e2e that
  blinds the CLI's `data_dir` prove the write lands in the daemon's live graph
  and not in a stale on-disk copy.
  **The guard was in two places, not one.** `remove_entity`
  (`src/reason.rs`) carries its own local-Fact immunity check, so a `force`
  that lifted only `forget_entity`'s outer guard would have counted and reported
  `removed_entities: 1` while removing nothing — success printed over a silent
  refusal. Both take `force`; every other caller, GC included
  (`src/tick_stigmergy.rs`), passes `false`, and that is the only bypass of the
  Fact guard in the tree. The response carries a third field, `kept_facts`,
  beyond the two the item asked for: without it a source made only of local
  Facts answers `removed_entities: 0`, which is indistinguishable from "that
  source was never ingested" — the refusal has to be observable or `--force` is
  undiscoverable and an incomplete deletion reads as a complete one.
- **Retention reaches the id read surface** — was item 91 `[retrieval]` (the
  second item to carry that number; the `[ingest]` one is still open), closed
  2026-07-21. Every claim in the item was re-verified against source first and
  all of them held. The item prescribed "one filter at one call site"; that was
  **not** what shipped, and the reason is the decision the item deferred. An
  explicit id names one row. The ranked path answers "what is true now", so it
  drops; `kern get <id>` answers "what is this row", and replying
  `thought not found` for a row that is on disk — and that GC never collects,
  since a non-superseded `Fact` is immune (`is_cold_victim`,
  `src/tick_stigmergy.rs:35-46`) — is a false statement the caller has no way to
  falsify. So the id path **serves and flags**: `entity_detail`
  (`src/mcp_tools_query.rs:372`) emits `expired` and `valid_until` whenever a
  retention is set, and `kern get` prints an `Expired:` line
  (`src/commands_graph_ops.rs:67`). Filtering lost because the surface item 9
  deliberately widened — prefix plus cold-tier fallback — would have been
  silently narrowed by it to nothing a caller could distinguish from a typo.
  Proven by revert: dropping the two lines in `entity_detail` fails
  `graph_ops::tests::the_id_path_flags_an_expired_thought_instead_of_hiding_it`
  on `left: Null, right: Bool(true)` and fails
  `tests/e2e/test_retention.py::test_an_expired_fact_is_served_by_id_but_flagged` on
  a real `kern get` printing the expired fact with no marker. The bi-temporal
  escape is now pinned at the call site too, not only on the predicate:
  `retrieve_drops_an_expired_claim_from_the_default_path`
  (`src/retrieval_query.rs:609`) runs the same corpus twice and asserts an
  `as_of` query still returns the since-expired claim; neutering the early
  return in `drop_expired` fails that half alone. (Item 18's ACL half of this
  history is moot since the 2026-07-22 removal decision.)
- **A deduped ingest carries its retention** — was item 88, closed 2026-07-21.
  There were two dedup gates and both swallowed it; both now funnel through one
  site. `accept::merge_duplicate` takes the incoming `valid_until` and calls
  `accept::merge_valid_until`, so the `find_duplicate` gate in `place.rs` and
  `commit_entity`'s `dup` branch reach the same rule, and the fresh-placement
  path calls it directly after accept. The rule is `resolve_valid_until` — `min`
  with `None` as +∞, decided over LWW because a TTL is a ceiling and `min`
  converges under the arbitrary replay order federation produces where
  last-writer does not. Accepted cost, stated in the tool schema: ingest can
  shorten a deadline and never lengthen one. The orphan delta the item predicted
  was real and is gone — the pre-accept stamp in `place.rs` minted a
  `PendingDelta` for an id gate 2 then discarded; the stamp moved *after* accept,
  onto the id that entered the graph. Proven by reverting each half separately:
  neutering gate 1 fails three `place.rs` tests and leaves the gate-2 test
  green, neutering gate 2 fails only the gate-2 test, and removing the
  `push_delta` fails all three delta assertions including the fresh-placement
  one. `tests/e2e/test_retention.py::test_a_deduped_ingest_still_applies_its_retention`
  fails loudly with the merge neutered. What this did *not* buy was item 91
  `[ingest]` — not the `[retrieval]` 91 listed above: the same gate still
  reported `committed` on a dedup and still left the discarded id in the
  lexical index. That closed separately, later the same day.
- **Per-source TTL has a writer** — was item 22, closed 2026-07-21. The reader
  (`score::drop_expired`) had been waiting for one; `valid_until` is now set at
  ingest from a `retention_secs` on the MCP `ingest` schema and a
  `kern ingest --retention-secs N` flag, through the single conversion
  `ingest::valid_until_from_retention` so the two entrances cannot drift. It
  reaches the entity on every path that creates one: MCP sync, MCP durable
  direct intake (`DirectJob` carries the resolved *instant*, since the job may
  sit a whole poll interval before draining), MCP RAM enqueue, and the CLI —
  and on the chunk path as well as the document path, which were two separate
  hardcoded `None`s. `tests/e2e/test_retention.py` proves the round trip against the
  real binary: recallable before the deadline, gone after, with a control fact
  that stays. What the item did *not* buy became items 88, 89 and 90; 88 — the
  dedup branch swallowing a retention — and 89 — the other two entrances, and
  the missing config key — both closed the same day, and what is still open is
  that `DirectJob` drops `valid_from` (90).
- **Retention reaches all four entrances, from a key a `kern.toml` can set** —
  was item 89, closed 2026-07-21. The `.txt` distillation path and the
  file-watcher sink had no caller to pass a flag: `drain_entry` cloned the
  queue's `Config` and overwrote only `valid_from`, and `KernFileWatcherSink`
  handed the worker `IngestRunConfig::default()` outright. Both now carry one,
  through `Config::with_retention` onto the existing
  `valid_until_from_retention` — still one duration→instant conversion, so the
  four entrances cannot drift. **The key could not go where this item said.** It
  named `IngestConfig`, but `load_with_user` refuses a user-written `[heat]`,
  `[ingest]` or `[retrieval]` section outright, so the half the item itself
  called load-bearing would have shipped unsettable; it went to `[intake]` and
  `[watcher]`, the sections that name the sources, and
  `a_real_kern_toml_can_set_per_source_retention` loads a real file rather than
  trusting the struct. Resolved per drain pass and per watched record, never at
  daemon start — hoisting it back above the intake loop collapses two
  transcripts queued two seconds apart onto one deadline and fails
  `the_poll_loop_resolves_its_deadline_per_pass_not_once_at_startup`. **Two
  things this did not buy.** The file-watcher half is unit-covered only —
  `WatcherConfig::enabled` is `false` by default and nothing in `tests/e2e/` starts a
  watcher, so `the_sink_stamps_the_configured_retention_on_what_it_ingests` is
  the whole proof for that entrance. And a durable `direct/` job still cannot
  inherit a standing policy: `drain_direct_once` overlays `job.valid_until` over
  the loop's config, and an absent flag and `--retention-secs 0` are both `None`
  on the wire, so per-call wins with no way to defer. Deliberate — the flag is
  the more specific statement — but it is a seam, not an absence.
- **The intake is visible and drivable** — was item 8, closed 2026-07-21.
  `kern intake` (alias `kern intake status`) prints pending with age, the last
  error for anything stuck, quarantined `failed/` entries and the `done` count;
  `kern intake drain` forces one pass, sharing `drain_once` with the daemon's
  loop so the two can never diverge (routed through the daemon since 2026-07-21;
  in-process when none is serving, so the CLI still works with no daemon).
  Building it exposed the real hole: the three paths that leave a delta queued
  **recorded no sidecar at all** — no `[reason]` endpoint, a reason model
  answering prose, and a transient read error. Those are exactly the
  retry-forever cases this item exists to surface, so each now writes why, and
  `a_transcript_left_queued_records_why_it_is_stuck` holds the line. The drain
  flushes through the same guarded retry as `cmd_ingest`, so a running daemon
  makes it a refused-flush-and-reload, never a clobber (item 9 still owns the
  lock).
- **A reason edge lifts its neighbour into the results** — was item 86, the
  instrument's first finding, closed 2026-07-21. The design question — how does
  a walk pay without letting a well-connected node outrank a direct match — is
  answered with bounded source-weighted traversal credit: every edge the walk
  examines credits its far endpoint with `source_score × edge_evidence`, once
  per (edge, endpoint), summed, weighted (×2), capped (0.5), and **clamped just
  below the strongest voucher's own walk score**, so a neighbour rides up
  behind what vouched for it, never past it. The clamp, not the weighting, is
  what protects direct answers — measured directly: unclamped, `recall@1` fell
  0.9167 → 0.7639. Root cause beneath cause 2 (`results` keeps max, so a seed
  score swallowed edge evidence): `link_entities` scored a deliberate link by
  `cosine(from, to)` — weakest exactly where the edge is the only evidence.
  Deliberate links now carry asserted confidence (user 1.0 / agent 0.95); auto
  similarity edges keep their measured cosine. The item's stated bar cited
  `recall@1` 0.9583, stale — that figure predates the answer-leg removal, whose
  own closure records the master baseline this was judged against: 0.9167 /
  0.9722 / 0.9462. Landed: all 8 pairs in the top 5
  (`test_a_reason_edge_makes_its_neighbour_reachable`, xfail marker removed),
  exact-match probe still rank 1, and recall *improved*: 0.9306 / 0.9722 /
  0.9471. Weighting variants swept (source² and edge-reliability² damping):
  linear source weighting was the only one that beat baseline.
- **The answer leg is deleted** — closes items 63, 68, and 80 (2026-07-21).
  Synthesis, HyDE, LLM rerank, and the query cache are gone; `query` returns
  passages/edges/chains and the calling agent synthesizes. Retrieval is
  LLM-free end to end, which is exactly the path item 1's instrument scores;
  e2e floors held unchanged through the removal (recall@1 0.9167, MRR 0.9462).

Each of these was listed as open at some point and is not. Kept as one line so a
future audit does not resurrect it; the proof is the citation.

Numbers are stable identifiers, retired on close — a closed item leaves its
number behind rather than compacting the list, because items cite each other by
number ("blocked on item 13") and renumbering would silently repoint them.

- **`(belief, uncertainty)` is already on `query`** — was item 23, closed on
  inspection 2026-07-21 rather than by building anything. The item said
  `conf_variance` is "surfaced nowhere"; every result payload carries both terms
  — `conf` (the beta mean) and `conf_uncertainty` (the variance) — in
  `base_entity_json` for the ranked list and in `entity_detail` for the id path
  (`src/mcp_tools_query.rs`), and has since the initial commit. Nothing was
  scheduled against a gap that never existed; what remains is item 65, which is
  about *ranking* on the lower bound, not surfacing it.
- **In-memory drops are documented and counted** — was item 5. Spill-before-drop
  is a guarantee of a persisted kern; with no store bound there is nowhere to spill
  to, so the victim is dropped outright. Intended, now stated in `README.md` and
  carried by `unspilled_drops` on all three health surfaces, so an in-memory
  deployment cannot read as a durable one.
- **Every fail-open path is counted** — was item 7. Dead embed endpoint, clock
  skew stalling GC, the `min_deliver_score` bypass and the 50k remote ceiling each
  carry a counter, a throttled log and a health field on MCP, RPC and `kern health`
  (CHANGELOG 2026-07-21). Fail-open is still the behaviour; it is no longer silent.
- **`commit_access` is rate-limited** — was item 16. Retrieval stamps every
  delivered result, so replaying one query pumped a thought's count and heat for
  free. One reinforcement per thought per minute; genuine reuse across a session
  still counts. The local twin of item 13's exposure, and the one that needed no
  peer.
- **An unauthenticated peer cannot reach a local row** — were items 13, 14 and 15.
  `ValidUntil`/`ReasonScore` LWW deltas confined to `remote-*` (the G-Counters keep
  their reach by design); entity bodies hash-checked against their claimed id;
  `handle_pulse` rejects unknown kern ids, confines deposits to `remote-*` and
  clamps strength. Item 13 was the hard edge that gated item 17.
- **`valid_until` is enforced on the default recall path** — was item 17.
  `drop_expired` runs on every retrieve, skipped when the query names its own
  instant so point-in-time history stays queryable. Unblocks item 22.
- **The retrieval instrument exists** — was item 1. `tests/e2e/` scored by
  `recall@1`/`recall@5`/`MRR` over a test-authored corpus with no LLM in the
  scoring loop: 0.9583 / 1.0000 / 0.9792, reproducible bit-for-bit. Floors make it
  a regression detector, not a quality claim; the bag-of-words fake embedder means
  it measures kern's machinery, not a real model's semantics. Both limits recorded
  (CHANGELOG 2026-07-21). Its first finding is item 86.
- **CI runs the lint gate and the e2e suite** — were items 71 and 72. `just check`
  and `just e2e` as jobs in `.github/workflows/ci.yml`. The banned-vocabulary step
  beside them could never fail and was fixed in the same change.
- **`.pi/update.sh` ships** — was item 73. It was gitignored, so the fresh-checkout
  guarantee did not exist; now the single tracked exception to `/.pi/*`, running
  `just docs-install` and `just e2e-install`.
- **A panicking tick task no longer kills maintenance** — was item 2. `catch_unwind`
  in `tick::start`, counted and surfaced on MCP, RPC and `kern health`. The GNN
  forward/backward chain is fallible end to end, so a failed propagation persists
  nothing (CHANGELOG 2026-07-21).
- **The embedding-model swap is caught** — was item 3. Model and dimension stamped
  on flush, checked at open, guarded fail-open on the query path.
- **`as_of` no longer lies over the cold tier** — was item 4. The temporal triple
  round-trips through `cold_spill`.
- **Cold evictions are counted and surfaced** — was item 6. The 50k FIFO bound
  stays; it is no longer invisible.
- **The detached daemon logs** — was item 10. Per-arg, owner-only, append-only
  `<data_dir>/logs/{hub,daemon}.log` via `src/config_detached_log.rs`, on both the
  hub-first path (`src/hub_node.rs`) and the direct `spawn_daemon` fallback
  (CHANGELOG 2026-07-21).
- **An invalid config stops startup** — was item 11. Exit 78 (`EX_CONFIG`) for
  both an unparseable file and a failed `validate()`; an absent config still
  defaults silently (`src/main.rs`). Still owed and tracked in item 84:
  `serve.mcp_addr` has no reader, and `src/commands_admin.rs` still defaults on a
  foreign root's config error.
- **Config scopes deep-merge per key** — was item 12. `merge_deep`
  (`src/config_io.rs`), arrays as leaves. A scope that redirects a section's `url`
  does not inherit that section's `key` (`src/config_secrets.rs`) — the tradeoff
  the merge decision failed to name up front (CHANGELOG 2026-07-21).
- **`docs_check.py` scans all four documentation directories** — was item 74.
  `docs/site/content/`, `docs/kern/`, `docs/oracle/` and `README.md`, 876
  references, with a `<!-- docs-check: historical -->` page marker and a
  same-line deletion escape so a record of what was removed is not forced to
  resolve. The item's two named casualties were already fixed before it was
  scheduled; the three it actually caught were live lies in `FEATURES.md` and
  `SPECIALISTS.md` (CHANGELOG 2026-07-21).
- **Pulse and Question senders are live.** `broadcast_pulse` / `broadcast_q` built
  in `start_gossip` (`src/commands.rs:1063-1144`), pulse wired into the maintenance
  tick (`:761`) and the `pulse` MCP tool (`src/mcp_tools_admin.rs:218`),
  `broadcast_q` invoked by `do_resolve` (`src/tick_tasks.rs:382`), `handle_question`
  live-dispatched (`src/gossip_handler.rs:44`).
- **`Fetch` is wired** — `wire_fetch` installs the handler at
  `src/commands.rs:1099`. Single-id, so it is not anti-entropy (item 36), but it
  is not dead.
- **`union_statements` never existed**; remote heat is no longer pinnable
  (`src/merge.rs:20`, applied `:153`).
- **The query cache already matches paraphrases.** `QueryCache::lookup` keyed on
  cosine ≥ `theta` (0.97) against the stored query vector; `lookup_text` was a
  pre-embed fast path, not the only key. Historical: the cache itself was
  deleted with the answer leg (2026-07-21).
- **There are no HNSW tombstones.** See item 27.
- **`Kind` cannot be claimed at the wire boundary**, and the premise that it
  could was wrong on all three counts. Stronger than when this was written: the
  `ingest` schema no longer takes a `kind` at all (`src/mcp_tools_mutate.rs`),
  so there is nothing left for the since-deleted `validate_kind` to reject;
  `Superseded` is an `EntityStatus`, not an `EntityKind`
  (`src/base_types.rs:19-28`), so it was never claimable; and a forged `Fact` is
  unreachable regardless, since the MCP path runs `clamp_confidence(p.conf,
  AGENT_SOURCE)` capping at `MAX_AI_CONFIDENCE` 0.95 and `kind` is *derived* from
  confidence, which needs 1.0 for `Fact` (`src/math.rs:205-210`,
  `src/base_constants.rs:69`). Only the CLI reaches `Fact`, via
  `clamp_confidence(1.0, "user")` (`src/commands_ingest_cmd.rs:60`).
- **`conf` is clamped to [0,1]** — `validate_conf` (`src/validate.rs:14`)
  called at `src/mcp_tools_mutate.rs:140`.
- **A prose-answering reason model no longer archives deltas having stored
  nothing.** `parse_claims` returns `Option`; a reply with no parseable JSON array
  leaves the delta queued for retry, a well-formed empty array still archives
  (`src/ingest_distill.rs`, guarded by `prose_reply_carrying_knowledge_is_not_lost`).
  The retry-forever tradeoff it introduces is surfaced by item 8.
- **The intake accepts anything readable as text**, routing by what the file is:
  `.txt` is a transcript and is distilled, everything else is a `Document` stored
  whole via the watcher's path, binary is quarantined into `failed/`.
- **One vocabulary** — `IntakeConfig`, `[intake]`, `.kern/intake/`,
  `spawn_intake`, `kern.intake`, with the legacy directory self-migrating on
  first start.
- **Automatic session capture is a non-goal**, closed by decision rather than by
  building a producer. Two caller-driven entries exist — MCP `ingest` (primary)
  and a drop into `.kern/intake/` (backup) — and kern ships no session hook.
- **Filtered ANN end-to-end** (all three seed sources on `is_active`); RRF at the
  answer layer; the `answer:false` sub-ms no-LLM path; the semantic query cache;
  the lock-scoped answer path. The A/B that accompanied this shipped a recall
  number from the deleted bench; the change stands, the number is withdrawn.
- **Durability: `snapshot_if_dirty` on the maintenance tick.** WAL rejected —
  LMDB already orders recovery. (Item 75 is the DiskANN path, which this does not
  cover.)
- **Hub phases 1, 2 and 4.** The supervisor (resolve / spawn / adopt / unload,
  hub-first `kern mcp`, graceful shutdown via `KernRpc::shutdown`); idle
  lifecycle (`HealthRes.idle_ms` stamped in the MCP dispatch core and every typed
  RPC method, reaper double-checking under the root lock, `--idle-unload-secs`
  default 1800, 0 disables, hub-owned nodes only); `kern hub merge <src> <dst>`
  offline CRDT union via `absorb_graph` with both daemons stopped and src never
  written; and hub auto-start (`[hub] auto_start = false` opts out, `kern hub
  stop` ends it over RPC). Cross-kern navigation beyond resolve/status remains
  future work. Phase 3 is item 47.
- **Chunk external ids are keyed on full source identity** (`source_id()` + chunk
  index, not the bare section); an identity-less source gets an empty external id
  and never supersedes. Item 48 is the remaining *dedup* half.
- **There is one typed transport surface, not two.** The former "kern_rpc +
  search with overlapping DTOs" item was wrong: `trnsprt` exposes `kern_rpc` and
  `hub_rpc` (`src/transport/src/lib.rs:20-21`), and `kern_rpc` is
  `health` / `shutdown` / `call_tool` / `list_tools`
  (`src/transport/src/kern_rpc/svc.rs:5-8`) — a generic envelope with no query DTOs
  to overlap. Nothing to kill. (`KernRpc` mirroring the MCP tool list 1:1 was
  carried as item 24's second half; that half is retired 2026-07-21 — verified
  false on this same reading.)

