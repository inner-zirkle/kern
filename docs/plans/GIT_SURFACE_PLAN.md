# Git Surface Plan — condensing kern's commands into a version-control shape

Status: **phases 0–1 shipped 2026-08-16** (aliases `grep`/`show`/`rm`/`note`;
`kern log` + `kern blame` on the CLI, backed by the daemon's `log` operation —
see the CHANGELOG entry of that date). Phases 2–4 (§6) remain open. This
document answers three questions: is a git-shaped command surface feasible, how
much would it cost to integrate, and what would it do to the surface.

Scope: the CLI (`src/commands/src/lib.rs`) and the MCP tool table
(`src/rpc/src/server.rs:126-143`). No retrieval, scoring or physics change.

---

## 0. The finding in one paragraph

Kern is already about 70% git-shaped and nobody named it. Entity ids are content
hashes, the supersede chain is a parent pointer plus a log entry, `.kern/intake/`
is a staging index, `mutation_epoch` is a version counter, `flush_guarded`
already refuses a non-fast-forward write, `hub` is a remote registry, `doctor` +
`repair` is `fsck` + `fsck --fix`, and `Source` + `producer_id` is blame data.
The renaming half of this project is nearly free. What is missing is exactly one
thing — an **operation log** — and everything git can do that kern cannot
(`revert`, `reset`, a `log` that sees deletions) is missing for that single
reason. The good news is that adding it does **not** wipe the store.

---

## 1. What already maps, and how honestly

Fidelity is graded: **exact** = same semantics, different name. **real** = the
mechanism exists and does the job, wiring is missing. **partial** = the pieces
exist but do not compose into the git behaviour. **missing** = nothing.

| git | kern today | where | fidelity |
| --- | --- | --- | --- |
| object id (sha) | entity id is a content hash | `src/rpc/src/server.rs:758` | exact |
| working tree | the live resident graph | `src/graph/src/graph.rs` | exact |
| index / staging area | `.kern/intake/` durable drop-dir queue, with pending / stuck / failed / done | `src/commands/src/commands_intake_cmd.rs:40-71` | **real** |
| `git add` | `kern ingest` (writes straight through, does not stage) | `src/commands/src/commands_ingest_cmd.rs` | partial |
| `git commit` | `kern intake drain` (drains, but does not group a batch) | `src/commands/src/commands_intake_cmd.rs:83` | partial |
| commit parent | `superseded_by` + a `ReasonKind::Supersedes` edge new→old | `src/graph/src/accept.rs:550-613` | **real** |
| commit date | bitemporal stamps: `created_at`, `valid_from`, `valid_to`, `invalidated_at` | `src/base/src/base_types.rs:301-327` | exact |
| non-fast-forward refusal | `FlushOutcome::RefusedStale { disk_epoch, expected }` | `src/store_core/src/lib.rs:583`, retried at `commands_intake_cmd.rs:186-211` | **real** |
| `git log` | the `events` MCP tool: created / superseded, opaque resumable cursor | `src/rpc/src/server.rs:828-916` | partial |
| `git blame` | `Source` URI + author + url, `producer_id`, supersede ancestry, `ReasonKind::Provenance` | `src/base/src/base_types.rs:133-163`, `:310` | **real** |
| `git show` | `kern get <id>` | `Commands::Get` | exact |
| `git grep` | `kern search` / `kern query` | `src/commands/src/commands_query.rs` | exact |
| `git rm` | `kern forget` (hard delete, Fact-guarded) | `src/graph/src/graph_ops.rs:33` | exact |
| `git mv` | `move` — MCP only, no CLI | `src/graph/src/reason.rs` | **real** |
| `git merge` | CRDT union: LWW by (lamport, producer) + GCounters, commutative and idempotent | `src/graph/src/merge.rs:70-229`, `src/base/src/crdt.rs:54` | **real** |
| `git remote` | `kern register <path>`, `kern hub {status,resolve,unload,merge,stop}` | `Commands::Register`, `HubAction` | **real** |
| `git fetch` / `pull` | `kern import`, `kern hub merge src dst` | `Commands::Import`, `HubAction::Merge` | **real** |
| `git bundle` | `kern export` (versioned JSON, survives a `FORMAT_VERSION` wipe) | `src/commands/src/commands_export.rs` | exact |
| `git gc` | `kern gc` + `kern compact` | `store_core::compact_dir` | exact |
| `git fsck` / `--fix` | `kern doctor --json` emits a manifest, `kern repair <manifest>` executes only what it authorizes | `src/commands/src/lib.rs:211-220` | exact (stricter than git) |
| `git reflog` | — | — | **missing** |
| `git revert` | — | — | **missing** |
| `git reset` | — | — | **missing** |
| `git diff` | — | — | **missing** |
| `git stash` | — | — | missing |
| branch / checkout | — | — | **deliberately out of scope**, see §5 |

Two things deserve calling out from that table.

**`repair` is better than `fsck --fix`.** `doctor --json` produces a findings
manifest and `repair` re-verifies each finding before acting on it — it does no
discovery of its own. That is a design git never got to. Keep it; rename it,
don't reshape it.

**Merge is conflict-free by construction.** `merge_entity` joins field by field
via LWW and GCounters. There is no conflict state, no `MERGE_HEAD`, no
resolution step. That is a genuine advantage over git and it is why §5 refuses
branches.

---

## 2. The one missing thing: there is no operation log

`tool_events` (`src/rpc/src/server.rs:828`) does not read a log. It **walks every
entity in every resident kern** on every poll, pushes a synthetic event for
`created_at` and another for `invalidated_at` when the row is superseded, sorts
the whole vector, then returns 100 rows (`:844-883`).

Three consequences:

1. **It can only ever report two kinds of change.** A `forget` leaves no trace,
   because `forget_entity` → `remove_entity` is a hard delete
   (`src/graph/src/graph_ops.rs:33-43`). A `link`, `promote`, `degrade`, `move`
   or `prune` is likewise invisible the moment it lands.
2. **Nothing is invertible.** `revert` and `reset` are not hard in kern — they
   are undefined, because no record of what an operation did survives it.
3. **It is O(all entities) per poll.** A range scan over a journal is O(page).
   The journal is not only a feature; it retires a scaling defect in the one
   history surface that already ships.

There is also a live correctness bug sitting in the same hole. `absorb_graph`
is a pure union: an incoming entity with no local host takes the `None => insert`
branch (`src/graph/src/merge.rs:169-193`). So a row forgotten locally and still
present on disk or on a peer is **silently resurrected** by the next stale-flush
reconcile or `hub merge`. Kern has no tombstones. Adding them for `revert` fixes
this as a side effect, which is the strongest single argument for phase 4.

---

## 3. Feasibility of the journal

### 3.1 The store change is additive — no wipe

`Store::open` creates four named LMDB databases behind `MAX_DBS = 4`
(`src/store_core/src/lib.rs:23`, `:346-349`). Adding a fifth `ops` database is:

- `const MAX_DBS: u32 = 5;`
- one more `env.create_database::<Bytes, Bytes>(&mut wtxn, Some(OPS_DB))?`

**`FORMAT_VERSION` does not need to move.** It gates value *encoding* —
`encode_at(FORMAT_VERSION, v)` at `:90`, checked at `:95` — not the set of
databases. Every existing `kern` / `cold` / `cold_vec` / `meta` value decodes
unchanged. Raising `max_dbs` on an existing LMDB env is legal and does not
rewrite the file.

This is the single most important feasibility fact in this document: **an
operation journal can be shipped without wiping anybody's graph.** Given that
`FORMAT_VERSION` is at 11 and the repo's stated policy is "versioned, never
migrated" (`src/lib.rs:13`), that is not a small thing.

### 3.2 The key is the epoch

```
key   = (epoch: u64 BE, seq: u32 BE)      // 12 bytes, lexicographic = chronological
value = bincode(Op { at, actor, kind, target, before, after })
```

The epoch counter kern already keeps (`mutation_epoch` / `flushed_epoch`,
`src/graph/src/graph.rs:164-165`; `Store::read_epoch`,
`src/store_core/src/lib.rs:432`) becomes the commit id. `kern log` is a reverse
range scan. `kern reflog` is the raw table. `gc` prunes the tail by age — the
bounded-prune pattern already exists as `cold_cap_amortized` (`:741`).

### 3.3 The emit sites are already centralized

`src/graph/src/graph_ops.rs:1-3` says it outright: these are "the mutations
shared by the CLI and MCP surfaces". Instrumenting them once covers both. The
full list is about ten call sites:

- `graph_ops.rs` — `forget_entity:33`, `prune_matching:51`, `forget_by_source:104`,
  `promote_entity:139`, `link_entities:158`, `degrade_entity_reasons:199`
- `accept.rs` — the accept path, `stamp_superseded:550`, `promote_unnamed:929`
- `reason.rs` — `move_entity`, `add_reason`, `remove_entity:160`

One extra LMDB put per mutation, inside the write txn that `flush_guarded`
already opens. At ~150 bytes per row and 10k mutations/day that is ~1.5 MB/day,
against a 16 GiB map size (`store_core/src/lib.rs:22`) and an existing
self-heal compaction path at 512 MiB (`commands/src/lib.rs:27`).

### 3.4 Invertibility: three tiers

**Invertible today, no schema change.** `link` (drop the reason id — `link_entities`
already returns it), `promote` (Active→Pending; `promote_entity:143` already
returns `Ok(false)` for a no-op, so it is idempotence-aware), `degrade` (store
the pre-decay scores in the op — `degrade_entity_reasons` already returns
`(decayed, removed)`).

**Invertible with a schema addition.** `forget`. Today it is destructive. It
needs a tombstone: `EntityStatus::Forgotten = 2` appended to the `#[repr(u8)]`
enum at `src/base/src/base_types.rs:74-78`. Appending a discriminant is
bincode-compatible — existing 0 and 1 decode unchanged. `remove_entity` marks
instead of deleting; the ANN eviction it already does
(`accept.rs:576` is the same pattern for supersede) stays; `gc` reaps tombstones
past a retention window; `merge_entity` learns to let a tombstone win. That last
line is the §2 bug fix.

**Not cleanly invertible.** `ingest`. Reverting one means unwinding placement,
kern spawn decisions and ANN inserts. Don't pretend otherwise — define
`revert` on an ingest as *tombstone the entities it produced*, which is what a
user means by it anyway.

---

## 4. What it does to the surface

Today: **29 top-level commands** (`Commands`, `src/commands/src/lib.rs:99-280`)
plus 14 leaf subcommands across `HubAction`, `IntakeAction`, `GravitonAction`,
`ClaimKindAction`, `UnnamedAction` — about **38 leaf verbs** on the CLI, against
**18 tools** on MCP (`src/rpc/src/server.rs:126-143`). They do not match:
`move` is MCP-only; `prune`, `doctor`, `repair`, `export`, `import`, `compress`,
`register`, `unnamed`, `reembed`, `profile`, `list`, `get` and `compact` are
CLI-only.

The two surfaces have already drifted further than that. The MCP server
currently installed on this machine still advertises `contract_grant` and
`sign`, which exist nowhere in the tree — federation was removed at
`FORMAT_VERSION = 11`. A single named lifecycle is partly a defence against
exactly this.

### The regroup

| proposed | absorbs | change |
| --- | --- | --- |
| `kern add` | `ingest` | rename; default to staging into intake rather than writing through |
| `kern commit` | `intake drain` | rename; group one drain into one epoch |
| `kern status` | `status`, `intake status`, `health` | merge 3 |
| `kern show [id]` | `get`, `list` | merge 2 — no argument means list |
| `kern grep` | `search`, `query`, `profile` | merge 3 behind `--mode` |
| `kern rm` | `forget`, `forget --source`, `prune` | merge 3 behind `--source` / `--grep` / `--dry-run` |
| `kern mv` | `move` | **new on the CLI**, exists in MCP |
| `kern log` | `events` | **new on the CLI**, journal-backed |
| `kern blame <id>` | — | **new**, fully derivable today |
| `kern revert <epoch\|op>` | — | **new**, needs the journal |
| `kern restore <id>` | `promote`, `degrade` | merge 2, plus un-forget |
| `kern note` | `link` | rename — a link *is* an annotation carrying a reason |
| `kern remote` | `register`, `hub {status,resolve,unload,merge,stop}` | merge 6 |
| `kern bundle` | `export`, `import`, `compress` | merge 3 |
| `kern fsck` | `doctor`, `repair`, `audit` | merge 3 behind `--fix` / `--content` |
| `kern gc` | `gc`, `compact`, `reembed` | merge 3 behind `--compact` / `--reembed` |
| `kern daemon` | `daemon` | keep |
| `kern graviton` · `kern claim-kind` · `kern unnamed` · `kern pulse` | — | **keep as domain nouns**, see §5 |

**29 top-level → 17 lifecycle verbs + 4 domain nouns.** Leaf count falls from
~38 to ~28 while gaining four capabilities that do not exist today (`log`,
`blame`, `revert`, and `mv` on the CLI at all).

The verb count is not the real win. The real win is that flags replace verbs —
`rm --grep` instead of a separate `prune` — and that anyone who knows git knows
kern's whole lifecycle without opening the docs.

---

## 5. What not to do

**Do not git-ify the physics.** `graviton`, `kern`, `pulse`, `heat` and
`claim_kind` have no git analogue, and forcing one lies about them. A graviton
is an attractor that pulls placement, not a tag. A kern is a spatial cluster
with radii and mass, not a line of development. The rule that keeps the whole
surface coherent: **git verbs for the lifecycle, domain nouns for the physics.**

**Do not add branches or checkout.** Kern's model is one converging graph whose
merge is conflict-free by construction (§1). Branches import divergence, and
divergence needs conflict resolution — git's hardest problem, deliberately
absent here. There is nothing to gain and a CRDT invariant to lose.

**Do not ship `commit` as a bare rename.** A drain is not atomic today; each
delta lands independently, so `commit` would name something that isn't one.
Either group a drain batch under a single epoch — nearly free, the counter
exists — or call it `kern drain` and skip the word. Grouping is the better
answer, because it makes `revert <epoch>` mean "undo that batch", which is the
thing a user actually reaches for.

---

## 6. Phasing

The first two phases are independently valuable and fully reversible. Phase 3 is
the commit point.

### Phase 0 — aliases only · ~1 day · no risk

`#[command(visible_alias = "...")]` on the existing `Commands` variants
(`src/commands/src/lib.rs:99-280`): `show`, `grep`, `rm`, `note`. Both spellings
work, no behaviour changes, every existing doc stays correct. Measures appetite
before anything is paid for.

### Phase 1 — `blame` and `log` on the CLI · ~2-3 days · no schema change

`kern blame <id>` is pure derivation from data that already exists: the `Source`
URI with its author and url, `producer_id`, `created_at`, the `superseded_by`
chain walked backwards, and the incoming `Provenance` / `Supersedes` edges.
`kern log` wraps `tool_events` through `route()` the way every other command in
`commands_route.rs` does.

Highest value per unit of risk in the whole plan, and it stands alone — if
nothing after this ever ships, kern still gained provenance and history on the
command line.

### Phase 2 — the regroup · ~1-2 weeks · mostly mechanical

Only `Commands` and `dispatch` change (`src/commands/src/lib.rs`, 1607 lines);
the nine `commands_*.rs` bodies keep their signatures. 32 `Commands::` references
tree-wide.

**The real cost is documentation, and it is the majority of this phase.** Sixteen
doc files reference CLI verbs, and `tests/docs_check.py` will not catch a single
stale one — it validates file paths, line anchors and page links, not command
names. Stale verbs would rot silently and invisibly. Budget the doc pass
accordingly, and consider teaching `docs_check.py` to parse the clap surface and
check backticked `kern <verb>` occurrences against it while the surface is
already being touched.

### Phase 3 — the journal · ~1-2 weeks · the commit point

New `ops` database, `MAX_DBS` 4→5, ~10 emit sites, no `FORMAT_VERSION` bump
(§3). Ships `kern reflog`, a journal-backed `kern log` that sees deletes and
links, the O(page) fix for the events feed, and one epoch per drain batch.

### Phase 4 — tombstones and `revert` · ~1-2 weeks

`EntityStatus::Forgotten`, `remove_entity` marks rather than deletes, `gc` reaps
past a retention window, `merge_entity` lets a tombstone win. Then `kern revert`
and `kern restore`. Closes the delete-resurrection hole in `absorb_graph` (§2) as
a side effect.

---

## 7. Open questions

- Should the journal record **MCP-originated** ops with the calling agent as
  `actor`? `Scoping { user_id, agent_id, session_id }`
  (`src/base/src/base_types.rs:278-283`) already carries the identity; wiring it
  into the op row would make `kern blame` answer "which agent wrote this",
  which is arguably the highest-value thing on this whole list for a
  multi-agent host.
- Does `kern add` staging-by-default break the ingest path's callers? Intake is
  config-gated (`cfg.intake.enabled`, printed as `[intake disabled in config]`
  at `commands_intake_cmd.rs:51-55`), so `add` would need a write-through
  fallback when it is off — which is exactly today's `ingest`.
- Retention policy for the journal: age, count, or epoch depth? Ties into
  whatever `revert` is allowed to reach back to.
