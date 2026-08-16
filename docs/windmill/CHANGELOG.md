# Changelog

<!-- docs-check: historical -->

- 2026-08-16 — every link inside the published `llms.txt` (and every
  page's own `.txt` twin — `howto/install-run.txt`, `decisions.txt`, …)
  pointed at `http://localhost:3000/...` instead of
  `https://inner-zirkle.github.io/kern/...`. Reported by feb from the live
  site, not caught by the doc sweep two entries below because that sweep
  read source, never fetched the deployed page. `lib/llm-txt.mjs`'s `site`
  const falls back to `http://localhost:${PORT ?? 3000}` when
  `NEXT_PUBLIC_SITE_URL` is unset, and `.github/workflows/docs.yml`'s build
  step set `NEXT_PUBLIC_BASE_PATH` but never `NEXT_PUBLIC_SITE_URL` — so
  every production build baked in the dev fallback, silently, for both the
  `scripts/gen-llm-txt.mjs` pre-build step and the `/llms.txt` route's own
  `llmsTxt()` call (which also defaults to `site` outside
  `NODE_ENV=development`). Fixed by setting `NEXT_PUBLIC_SITE_URL:
  https://inner-zirkle.github.io` alongside the existing base-path env var.
  Verified: `NEXT_PUBLIC_SITE_URL=https://inner-zirkle.github.io
  NEXT_PUBLIC_BASE_PATH=/kern npm run build` locally, then `grep -c
  localhost out/llms.txt` → `0` and `out/howto/install-run.txt` opens with
  `https://inner-zirkle.github.io/kern/howto/install-run/`.
  Decided by: feb (reported the live bug directly); verify-before-claiming
  (reproduced the exact env-var gap in `docs.yml` rather than guessing at a
  client-side routing cause, then confirmed the fixed build's output).

- 2026-08-16 — the v2.0.0 release build (previous entry) is what finally
  exposed three portability bugs that had silently sat in every `release.yml`
  run since v1.1.0, each one failing 5-8 of the 16 matrix targets while the
  workflow still ended in a partial GitHub Release, which read as "it
  shipped." All three: `src/transport/src/wire.rs` — the pre-MCP tcp/unix/
  http/stdio framing module the old MCP transport dispatched through —
  `use std::os::unix::net::UnixListener` with no `#[cfg(unix)]` guard, so
  every Windows target (`x86_64-pc-windows-msvc`, `i686-pc-windows-msvc`,
  `x86_64-pc-windows-gnu`, `aarch64-pc-windows-msvc`) failed outright with
  `E0433: cannot find unix in os`. Grepped for callers of `wire::serve`/
  `wire::select` across the whole workspace: zero, on either side of the MCP
  removal — the module was already fully orphaned, not merely
  Windows-unsafe, so it's deleted rather than `#[cfg(unix)]`-gated (`typed.rs`
  + `kern_rpc.rs` are the real substrate both RPC contracts run on; see
  updated FEATURES.md §18). One layer down, the same fix exposed
  `typed.rs`'s `SUN_LEN_MAX` (Unix-socket-path-length constant, used only
  inside a `#[cfg(unix)]` block) declared with no matching guard — a bare
  `-D warnings` `never used` on Windows once `wire.rs` stopped shadowing it.
  Separately, on every 32-bit target (`armv7-unknown-linux-gnueabihf`,
  `arm-unknown-linux-gnueabihf`, and `i686-pc-windows-msvc` again):
  `store_core::MAP_SIZE = 16 * 1024 * 1024 * 1024`, LMDB's virtual-address-
  space reservation (free on 64-bit — mmap'd, not allocated), overflows
  `usize` at const-eval on a 32-bit target before the build can even reach a
  linker (`E0080`) — and 16 GiB would not fit in a 32-bit process's address
  space regardless. Now `#[cfg(target_pointer_width = "64")]` 16 GiB /
  `#[cfg(not(...))]` 1 GiB. Verified: `cargo check -p transport --target
  x86_64-pc-windows-gnu` and `cargo check -p store_core --target
  i686-unknown-linux-gnu` both clear the specific errors reproduced from the
  v1.1.0–v1.3.0 CI logs (full linking not locally reproducible — no cross
  linker installed — but both were compile-time-const/type errors, not link
  errors, and both are gone); `cargo check --workspace --all-targets` and
  `cargo test --workspace --lib` (every crate, 0 failed) stay green natively.
  **A fourth bug those three were hiding**: the re-cut `v2.0.0` tag's real
  CI run still failed all 4 Windows targets — `commands/src/lib.rs`'s
  `let takeover = Arc::new(AtomicBool::new(false))` was declared
  unconditionally but read only inside two `#[cfg(unix)]` blocks (the hot
  -reload takeover flag), so it never appeared in the old v1.1.0–v1.3.0 logs
  at all: those runs died at the `wire.rs` `UnixListener` import before the
  build ever reached this line. Gated the `let` behind `#[cfg(unix)]` too.
  The honest caveat on the "verified" claim above: *local* verification
  checked the specific error signatures already on record, not "every error
  a real Windows toolchain would find" — this one was invisible until the
  actual re-tagged CI run surfaced it, which is exactly why the tag was
  re-cut on the fix rather than trusted on the local check alone.
  Decided by: feb (user-directed, folded into "release it"); fix-the-root
  (deleted the orphaned module rather than patching a `#[cfg(unix)]` onto
  code nothing calls); verify-before-claiming (traced actual CI logs from the
  three prior failed releases and the fresh re-run rather than guessing at
  the failure mode, and named the limit of the local check rather than
  letting the first "green" claim stand uncorrected).

- 2026-08-16 — released v2.0.0: kern leaves alpha. `AGENTS.md`'s "Alpha — no
  compatibility" section is now "Format compatibility" — a store written by
  the previous release must keep opening (the one-hop migration two entries
  below is what makes that true); the wire RPC still gets no cross-release
  promise, decoded tolerantly instead. This was the blocker the 2026-08-07
  release-readiness audit named for staying alpha ("leaving alpha is a
  promise of format stability... no migrations"); it no longer applies.
  Cargo.toml `1.4.0` → `2.0.0` — a major bump because two things broke
  compatibility with the v1.x line in the same tree: the MCP surface and the
  gossip/federation surface both removed outright (see the two same-day
  entries below), which strands anyone who wired a v1.x client against
  either. Alongside the version bump, a documentation pass: every public
  docs-site page and `docs/llms.md` still described `kern mcp` — wiring JSON,
  a sixteen-tool list, an HTTP/token auth section — as live (`howto/mcp.mdx`
  deleted outright, ten other pages edited); `docs/kern/README.md`'s research-note
  links were broken (pointed at `docs/kern/*.md`, the files live in `docs/`
  since a1655c5 canonicalized the layout — six dead links fixed); README's
  quickstart still claimed store formats reject with no migration path.
  One real gap surfaced in the sweep, not fixed here: `kern query`'s CLI
  flags (`--mode`/`--k`/`--exclude-pending`/`--all`/`--live`) never grew the
  filter surface the old `query` MCP tool exposed (`kind`, `source`, time
  range, `min_conf`, `as_of`, `include_history`) — the daemon's `query`
  operation may still accept them, nothing on the CLI sends them. Docs now
  describe only what `kern query` actually takes; restoring the rest is
  follow-up work, not claimed as done.
  Decided by: feb (user-directed — "bring everything up to a releasable
  version... it is working"); verify-before-claiming (every compatibility and
  surface claim in this entry checked against source before being written,
  including the query-filter gap, which asserting an invented `--kind` flag
  would have hidden).

- 2026-08-16 — `kern query --all` searched every kern on the machine except the
  one you were standing in. The hub only knows roots it has resolved or that
  announced themselves at daemon boot, so a project that never ran a daemon was
  absent from the registry and silently skipped — which reads as an empty store,
  not as a missing registration. `cmd_search_all`
  (`src/commands/src/commands_query.rs`) now resolves the caller's own root
  before it fans out, registering it; failure to register is reported and the
  other kerns still answer. Not under `--live`, which is the caller saying "wake
  nothing" — there the local kern joins only if it is already serving.
  In the same pass, `--k` gained one default across every mode: the retrieval
  preset's delivery cap. `--all` and `--mode vector` had inherited 5 from the
  `search` command they absorbed, so the same question asked three ways answered
  with three different hit counts and the cross-kern read looked like it had
  found almost nothing (5 hits against a 1984-thought store).
  Decided by: feb (user-directed — "still so little hits?"); name-the-tradeoff
  (registering spawns a daemon for the local project, which is what `--all`
  already does for every cold kern it wakes; `--live` is the opt-out).

- 2026-08-16 — a store written by the previous build is migrated, not wiped.
  This **amends the alpha policy** in `AGENTS.md` ("no migration paths, no
  legacy decode fallbacks... old stores are wiped and reingested"), which was
  affordable while every store was empty and stopped being affordable the moment
  one held a corpus. What landed: frozen decode-only snapshots of the outgoing
  layout (`src/store_core/src/legacy.rs`) that `decode_kern_row` and
  `decode_cold` reach for on an older version byte and convert forward; meta
  rows through `decode_layout_stable`, since `network_id` → `replica_id` is a
  rename and bincode is positional; `kern migrate` to rewrite kern rows, the
  cold tier and the meta rows in one writer-lock-guarded, idempotent pass; a
  `format_older_than_build` finding in `kern doctor`; and
  `store_core::migrated_from()` as the one signal both read. A read converts in
  memory only — the disk rows stay old until something writes, so recall works
  immediately and nothing is rewritten behind the caller.
  Proven end to end against a real v10 store written by a build of 6177100 (the
  last commit before the uncommitted v11 bump) in a throwaway worktree —
  `git worktree add <dir> 6177100 --detach && cargo build --bin kern` reproduces
  the writer: two thoughts and an edge read back with their text intact, a write
  rewrote the rows as v11, and the old binary then refused the store — one-way,
  as intended.
  The load-bearing discovery is that **the version byte was not trustworthy**:
  f60fbce added the persisted `Entity.trust_tier` without bumping
  `FORMAT_VERSION`, so two incompatible `Entity` layouts both call themselves
  version 10. The decoders therefore try the candidates and require a non-empty
  id (a run of zeros decodes as a structurally valid empty kern — "it parsed" is
  not evidence). `tests/layout_guard.rs` is what keeps the byte honest from here:
  it pins an FNV checksum of the encoded `StoredKern` and `ColdRow`, so a
  persisted field that moves fails the build with the three steps a bump owes.
  Decided by: feb (user-directed — "can we also build a migration?"; "if we
  detect lower stores we can just migrate them"); fix-the-root (the guard, not
  just the decoder — a migration is only as honest as the version it keys on).

- 2026-08-16 — the CLI is one surface, described and answerable. Three
  changes, one decision. **Every subcommand, subaction and argument carries a
  description** (`Commands`/`*Action`/`EmbedArgs`/`LlmArgs`,
  `src/commands/src/lib.rs`), under a one-line header carrying the version
  (`kern v<version> - adaptive knowledge graph`, built from
  `CARGO_PKG_VERSION` so it cannot drift from `--version`) and above nothing
  else — no examples block, and no underline under the section headers
  (`help_styles()`) — declared in the order a project meets them —
  twelve verbs and nearly every flag printed a blank column in `--help`
  before, which is an answer the reader had to go read the source for; the
  three daemon-plumbing globals (`-d/--daemon`, `--reason-url`,
  `--reason-model`) are hidden, since they are what kern passes to itself and
  `kern daemon` is the way in. **Failures exit non-zero**: one channel
  (`commands_exit.rs`) prints `kern <command>: <message>` to stderr — never
  stdout, the answer channel — and sets the flag `main` turns into exit 1
  (config errors keep sysexits 78). Every `cmd_*` returns `()`, so a failed
  `get`, `repair` or `import` printed its complaint and exited 0, and anything
  scripting kern had to grep stderr to tell a miss from a hit. **Four verbs
  absorbed their near-duplicates**: `search` → `query --mode vector` (the bare
  nearest-neighbour local read) plus `query --all` (the hub fan-out), with
  `--k` now binding every mode; `blame <id>` → `log <id>` (they were literally
  the same `log_report` walk); `prune <pattern>` → `forget --match <pattern>`,
  which also gives `--dry-run` to `--source`; `compact` → `gc`, which already
  reaped *and* compacted, so the separate verb only ever offered the half that
  frees no disk. 33 top-level commands became 28. No aliases kept for the four
  (alpha policy). Pinned end to end by `tests/e2e/test_cli_surface.py` — exit
  status, the `kern <command>:` prefix, and each absorbed capability answering
  under its new spelling. Four defects the end-to-end sweep of every verb then
  turned up, fixed in the same pass: `register <typo>` **created** an empty
  LMDB env at the bad path (`Store::open` creates what it opens) and reported
  success — it now checks for a directory holding `data.mdb` before opening
  anything, and reports the kern/thought counts it absorbed; the per-id miss
  was worded four ways across `get`/`log`/`degrade`/`promote` and is now one
  (`no thought with id <id>`, `per_id_error`); a per-id `forget` refused by the
  Fact guard — the common case, since everything `kern ingest` writes is a Fact
  — now names the way through (`--match ... --force`) instead of dead-ending;
  and `query --all` with no hits printed "across 1 kern(s)" from
  `skipped.len().max(1)`, a count of the kerns that *failed*.
  Decided by: feb (user-directed — "as simple, professional and smooth as
  possible"); name-the-tradeoff (two spellings for one job cost a reader a
  decision every time, and the cost of removing them is one relearn).

- 2026-08-16 — a bare `kern` prints help instead of booting the daemon
  (`src/main.rs`): a git-shaped CLI that quietly becomes a long-running
  process on a typo is a trap, and the help now runs before the config
  loads so a broken `kern.toml` cannot get between the user and the usage
  text. The daemon is reachable only by name — `kern daemon`, or the
  `--daemon` flag the hub's spawn and the detached-spawn paths pass.
  Decided by: feb (user-directed).

- 2026-08-16 — git surface, phases 0+1 of `docs/plans/GIT_SURFACE_PLAN.md`
  (the 2026-08-16 investigation, endorsed as the roadmap; phases 2–4 — the
  verb regroup, the operation journal, tombstones/`revert` — stay open).
  Phase 0: clap `visible_alias`es on the existing verbs — `kern grep` =
  `query`, `kern show` = `get`, `kern rm` = `forget`, `kern note` = `link`
  (`src/commands/src/lib.rs`) — both spellings work, no behaviour change.
  Phase 1: a 19th daemon operation, `log` (`invoke("log", {id?, limit?})` —
  `tool_log`, `src/rpc/src/server.rs`), with `log_report` shared between the
  daemon operation and the CLI's no-daemon fallback (`cmd_log`,
  `src/commands/src/commands_graph_ops.rs`) so the two cannot disagree about
  what history means. Bare `kern log [--limit N]` is the machine history
  derived from the bitemporal stamps the graph already keeps — `added` and
  `superseded` rows, newest first, capped (`LOG_DEFAULT_LIMIT` 20, created
  before superseded on an equal stamp, the events feed's tie-break);
  `kern log <id>` / `kern blame <id>` is one thought's revision chain — head
  first, then the `Supersedes` walk (cold tier reached for evicted
  revisions), each revision carrying its source URI, its created/invalidated
  stamps (UTC at minute resolution, `util::datetime_string`) and the
  `Supersedes` edge's text as the recorded why. Deliberately NOT visible yet:
  `forget`, `link`, `degrade` and every other mutation leave no bitemporal
  trace, so `log` cannot show them — that is the phase-3 operation journal,
  per the plan (which also retires the O(all-entities) walk the events feed
  still does). Pinned by `mod log_tests` (`src/rpc/src/server.rs`).
  Decided by: feb (user-directed — git for knowledge: the lifecycle in git
  verbs, the physics in domain nouns).

- 2026-08-16 — local federation: the machine hub is now the machine-wide
  knowledge broker. A persistent root registry (`src/hub/src/hub_registry.rs`
  — `$XDG_STATE_HOME/kern/hub-roots.json`, fallback `~/.local/state/kern/`,
  Windows `LOCALAPPDATA`; atomic temp+rename writes, tolerant open) records
  every root a hub `resolve` touches; the hub reaper (30s cadence,
  `spawn_reaper`, `src/hub/src/lib.rs`) prunes roots whose directory vanished
  and harvests per-node stats (entities, kerns, `data.mdb` bytes) from live
  daemons' health answers — a cold root keeps the last harvest, never a guess.
  `kern hub status` now lists every known kern, loaded or cold,
  importance-sorted (entities desc, then data bytes) — `HubStatusRes.known` /
  `KnownRoot` (`src/transport/src/hub_rpc.rs`), printed by `cmd_hub`
  (`src/commands/src/commands_admin.rs`). Cross-kern search: `HubRpc::search`
  (`SearchReq`/`SearchHit`/`RootErr`/`SearchRes`, same file) fans the query
  out on a `JoinSet` to every registered root through the same resolve path
  clients use — cold kerns are woken unless `live_only`, each root that could
  not answer comes back named in `skipped`, hits merge score-descending and
  cap at `k`. Two surfaces: `kern search --all [--live] [--k N]`
  (`cmd_search_all`, `src/commands/src/commands_query.rs:144`) and the daemon
  operation `search` (`tool_search`, `src/rpc/src/server.rs:661`), which hands
  the query to the hub and answers local-only with `fanout: false` plus a note
  when no hub is reachable. A booting daemon self-registers:
  `register_with_hub` (`src/commands/src/commands_admin.rs:777`, called from
  `run_server`) announces its root, auto-starting the hub per `[hub]
  auto_start`, so hand-started daemons appear in the registry too. Explicitly
  out of scope: cross-machine/SSH federation — the broker only reaches stores
  this machine already knows the location of.
  Decided by: feb (user-directed — one machine, one broker, every kern findable).

- 2026-08-16 — removed the MCP surface: agents drive the CLI. Deleted the
  `mcp` crate whole (tools, resources, prompts, hand-written schemas, stdio
  server), the `kern mcp` subcommand with its proxy/standalone/auto-restart
  machinery (`commands_mcp_cmd.rs`, `claim_standalone`, `replace_if_stale`),
  MCP-over-HTTP/SSE, the transport MCP envelope (`transport/mcp.rs`,
  `transport/http.rs`) and `.mcp.json` self-registration. What replaced it:
  every CLI verb is a thin dispatch to the long-running per-root daemon over
  the typed `KernRpc` — the contract is three methods,
  `health`/`shutdown`/`invoke(name, args) -> JSON`
  (`src/transport/src/kern_rpc.rs:270-273`); the handler wraps the daemon's
  core `rpc::Server` (`src/rpc/src/lib.rs`); and the operation surface — 18
  named operations, `query`..`setup` — is the `invoke` match in
  `src/rpc/src/server.rs:121-143`, every operation returning plain JSON with
  no envelope. The token handshake went with it: no auth frame and no
  `mcp-token` on the RPC — socket ownership is the whole access model
  (`require_owned_by_caller` uid check + `SO_PEERCRED`
  `require_peer_is_caller`, `src/transport/src/typed.rs:561`/`:607`, enforced
  on both connect and bind), and the config followed: the `[serve]` table
  (`mcp_addr`, token minting) and the `[hub] auto_restart` key (its only
  reader was `kern mcp`'s attach path) are gone from `Config` entirely.
  Agent trust is unchanged in kind: the `ingest` operation still clamps
  against `AGENT_SOURCE` and `link` against `MAX_AI_CONFIDENCE`, while the
  CLI's local no-daemon fallback still mints at user trust. Known leftovers,
  recorded not hidden: a `kern mcp` mention survives in
  `src/store_core/src/lock.rs`'s header comment, and `wire.rs:83` still
  parses `--mcp` as a stdio alias. Decided by: feb (user-directed — the CLI
  is the agent surface; one dispatch core, `rpc::Server::invoke`).

- 2026-08-16 — removed network federation: kern is local memory. Deleted the
  `src/gossip` crate whole (peer ring, ed25519 peer identity, UDP/TCP
  transport, seen-set, rate limits, ledger, contracts/grants, sealed-payload
  privacy, subscription fan-out) and everything that existed only to serve it —
  `[gossip]` config and `[[gossip.contracts]]`, the `GOSSIP_*`/`LEDGER_*`
  constants, the MCP `sign` and `contract_grant` delegate tools, `kern peers`,
  the pulse/question broadcast plumbing through `tick_loop`/`store`/`mcp`, and
  the `PendingDelta` queue (its only consumer was `start_delta_flush`, so it
  was an unbounded map nobody drained). The unauthenticated-peer trust boundary
  went with it rather than staying dormant: `is_remote_kern_id`,
  `apply_remote_trust`, `remote_trust_weight`, the `UNTRUSTED` chain tagging,
  `GOSSIP_REMOTE_KERN_ENTITY_CAP`, `Reason.to_net_id`, `Kern::is_remote`, and
  the remote carve-outs in PageRank, fact-immunity, stigmergy GC and the seed
  gate — with no peer, no kern can be remote, so every one of those branches
  was dead weight pretending to be a control. What stayed and why:
  `merge_entity`/`merge_reason`/`absorb_graph` are the external-commit
  reconcile path, not federation, so the CRDT joins survive under honest names
  (`merge_remote_entity` → `merge_entity_into`, `GraphGnn.network_id` →
  `replica_id`); `gossip::identity` was never peer identity but daemon
  lifecycle (build/config fingerprints, uptime, self-watch, takeover, successor
  spawn) and moved to its own `src/identity` crate. Cross-project reach is
  unchanged and was never federation: `store::Registry`, the machine hub, and
  `kern register`. `FORMAT_VERSION` 10 → 11 (alpha: wipe and reingest) —
  dropping `to_net_id` rehashes every reason id and `GraphMeta.network_id` is
  renamed. Docs follow the code: `docs/{crdts-federation,FEDERATION-SECURITY,
  fl-vs-knids-federation}.md` and `docs/plans/FEDERATION_PLAN.md` deleted,
  `FEATURES.md` §15 and ROADMAP Tier 5 (items 33-47) cut, VISION's federation
  principle replaced with "memory is local, and stays local". Verified: 476
  tests green across `base config math graph retrieval tick tick_loop store
  health identity` plus 114 in `ingest`; `mcp`/`rpc`/`transport` were mid-edit
  by a concurrent process at the time of writing and were not re-run.
  Decided by: feb (user-directed — kern is local memory, interconnected across
  repos through stores this machine already knows the location of).

- 2026-08-15 — nine subsystems adapted from mnemosyne (MIT), user-directed
  (FEATURES.md §24): a deterministic hygiene core (new `src/hygiene` crate)
  behind a write-time gate (`[hygiene] gate = off|warn|strict`, new
  `OutcomeStatus::Rejected` the durable legs archive instead of retrying) and
  a stored-content audit (`kern audit` + MCP `audit`; archive = the existing
  `ReviewState::Pending` hold, secrets flagged by label and never
  bulk-deleted); regex query-intent classification biasing the hybrid RRF
  fusion (`retrieval.intent_enabled`, General = bit-identical off); Weibull
  decay curves per distilled claim kind (unlabelled entities keep the exact
  exponential); `kern export`/`kern import` (versioned JSON, CRDT-union
  import with `hub merge` semantics, bi-temporal clocks in a side map, embed
  stamp guarded); read-only `kern doctor` + fail-closed `kern repair` that
  executes only manifest-authorized actions; a two-tier daemon query cache
  (text→embedding, args+mutation-epoch→results) with health counters; Beta
  seeding scaled by channel veracity (inline 1.0, session 0.7, file/ticket
  0.6, agent 0.8 — evidence strength, not the estimate); and a BEAM eval
  runner (`just eval-beam`) encoding mnemosyne's own harness-integrity
  postmortem. The cache surfaced a real epoch hole: forget/move/merge/degrade
  mutate `kerns` directly and never bumped `mutation_epoch` — a forgotten
  thought kept serving from cache (caught by
  `test_forget_source_lands_in_the_serving_daemon_not_on_the_clis_disk`);
  those four paths now bump explicitly, the invariant is documented on
  `bump_mutation_epoch`, and the item-25 pin test now asserts the new
  contract (chokepoints bump; access stamps and raw map writes stay silent).
  Named tradeoff: more epoch movement means more adjacency-cache
  invalidations and snapshot triggers — both err toward correctness.
  Verified: `cargo test --workspace` 1140 passed 0 failed, clippy and fmt
  clean; e2e green except `test_a_reason_edge_makes_its_neighbour_reachable`,
  which fails identically at the pre-change base commit and at origin HEAD
  in clean worktrees — pre-existing on this machine, not introduced here.
  Decided by: feb (user-directed adoption), verify-before-claiming,
  name-the-tradeoff. Supersedes: nothing — export also softens the alpha
  wipe policy's cost without touching the policy.

- 2026-08-14 — daemon socket bind survives a deep `XDG_RUNTIME_DIR`: the Unix
  socket path exceeded SUN_LEN (~104) under long tmpdirs (CI, nested
  worktrees), killing the bind with "path must be shorter than SUN_LEN".
  `Endpoint::scoped` now falls back to `/tmp/kern-<tag>-<user>.sock` when the
  runtime-dir path is too long; daemon and clients share the env, so both
  resolve the same endpoint. e2e harness gives the daemon a short runtime dir
  (`/tmp/kern-test-<pid>-<ns>`), which also un-flakes the GNN recall test on
  deep pytest tmpdirs. Verified: recall gates still green — CLI recall@1
  0.9861/recall@5 1.000/MRR 0.9931, GNN recall@1 0.9861/MRR 0.9931, both above
  their floors; full quality suite 22 passed; `cargo test --workspace` 1109
  passed, 0 failed. Decided by: measure-first — the flake was socket length,
  not retrieval quality.

- 2026-08-14 — retrieval recall/plumbing fix (docs/plans/RECALL_PLAN.md):
  the ~4.5s per-CLI-invocation cost was the resident HNSW rebuild of three
  indexes at process start, racing pi's 3s/5s tool timeouts. Load now opens
  mmap'd DiskANN snapshots (entity/gnn/reason) stamped with the store epoch;
  a changed store reconciles the diff into the delta overlay instead of
  rebuilding (tombstone removed ids, insert changed/new vectors, amortized
  full rebuild when the diff outgrows the snapshot). `from_saved_with_mode`
  spills by default. Load: 4.5s → 0.09s. New `kern prune --pattern
  [--source] [--dry-run] [--force]` subcommand for in-process data hygiene.
  `lexical_top_boost` defaults to 0.5 (exact-term matches float above
  embedding neighbours). Restored KERN_DIR env support (data_dir =
  `$KERN_DIR/data`) — the source had lost it while the installed binary
  honored it. `kern status` lists sibling stores with a KERN_DIR pin hint.
  Workspace suite: 1109 tests pass. Decided by: measure-first — the
  pipeline was 11ms; the failure was load+timeout+noise, not retrieval math.

- 2026-08-11 — deleted dead re-export `bind_embed_model` in commands/src/lib.rs (zero callers in commands crate; only used internally by bootstrap).

- 2026-08-11 — deleted two dead files in retrieval: `retrieval_importance_index.rs` + `test_optimization.rs` (neither declared in lib.rs, referenced nonexistent functions, never compiled; -380 lines). Also deleted five dead `test_support` re-exports + `#[allow(unused_imports)]` escape hatch in `commands/src/test_helpers.rs` (zero callers; -6 lines).

- 2026-08-07 — deflaked 3 parallel-run test races: (1) `ingest_queue_refused` in health payload — switched auth-gate envelope test to `graviton list` (owned per-call state, no other test touches it), (2) `cold_tier_pinned_at_capacity` warn-count — removed flaky tracing-subscriber layer interception, verified throttle independently in util, (3) `the_poll_loop_resolves_its_deadline` gap assertion — tolerance 1s to 500ms (clock-stepped box). Also fixed `tokio start_paused` missing in rpc+transport dev-deps (workspace unified the feature but standalone build broke). Decided by: parallel-run flake audit (10-workspace-run gate each).

- 2026-08-07 — released v1.4.0, still alpha. Version bumped 1.3.0→1.4.0 and FEATURES.md restamped to the post-split tree (128 `.rs` files across 24 crates, was 180 flat; ~63.6k LoC; reconciled 2026-08-07). Alpha wording in `AGENTS.md` and `README.md:231` deliberately unchanged: leaving alpha is a promise of format stability, and the live policy is still FORMAT_VERSION bump = wipe and reingest, no migrations. Shipping the cleanup does not require making that promise. Decided by: name-the-tradeoff (user chose tag-as-alpha over lowering the recall floor or writing a migration policy under pressure).

- 2026-08-07 — retargeted every stale nested-module path reference (`src/base/store.rs`-style) left behind by the src/ flattening to the flat layout (`src/store_core/src/lib.rs`-style) across `README.md`, `AGENTS.md`, `docs/*.md`, `docs/plans/`, and the present-tense windmill files (`ROADMAP.md`, `FEATURES.md`, `SPECIALISTS.md` — scopes now glob the flat prefixes). Historical text (this changelog, ideas.md flatten narratives, the absorbed `src/base/cold.rs` mention) untouched. Decided by: single-crate-fold (user-directed full src/ flattening).

- 2026-08-07 — flattened `transport/kern_rpc` nested subdir: 4 `src/transport/kern_rpc/{auth,client_local,dto,svc}.rs` → `src/transport_kern_rpc_{auth,client_local,dto,svc}.rs` at src/ root. `kern_rpc/mod.rs` → `src/transport_kern_rpc.rs` shim re-exporting auth/client_local/dto/svc + items (present_auth..KernRpcClient). lib.rs gained 5 `pub mod transport_kern_rpc*`. transport/mod.rs: `pub mod kern_rpc;`→`pub use crate::transport_kern_rpc as kern_rpc;`. `mod http;` widened to `pub(crate)` (auth.rs now a crate-root sibling needs crate::transport::http::ct_eq). Build green, 1096 tests pass, guards 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-07 — flattened `transport/hub_rpc` nested subdir: 3 `src/transport/hub_rpc/{client,dto,svc}.rs` → `src/transport_hub_rpc_{client,dto,svc}.rs` at src/ root. `hub_rpc/mod.rs` → `src/transport/src/hub_rpc.rs` shim re-exporting client/dto/svc + items (HubStatusRes..HubRpcClient) so `crate::transport::hub_rpc::X` resolves unchanged. lib.rs gained 4 `pub mod transport_hub_rpc*`. transport/mod.rs: `pub mod hub_rpc;`→`pub use crate::transport_hub_rpc as hub_rpc;`. Rewrites: super::svc/dto → crate::transport_hub_rpc_*. Build green, 1096 tests pass, guards 0. Decided by: single-crate-fold (user-directed full src/ flattening).

- 2026-08-07 — flattened `retrieval` subdir: 9 `src/retrieval/*.rs` → `src/retrieval_*.rs` at src/ root (prefix-rename, dodges merge collision). `retrieval.rs` kept as shim re-exporting all 9 submodules + EmbedFunc/LlmFunc so ~11 external `crate::retrieval::score::X`/`crate::retrieval::query::X`/`crate::retrieval::seed::X`/`crate::retrieval::LlmFunc` refs resolve unchanged. lib.rs gained 9 `pub mod retrieval_*`. No rewrites in moved files needed. Build green, 1096 tests pass, guards 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flattened `mcp` subdir: 10 `src/mcp/*.rs` → `src/mcp_*.rs` at src/ root (prefix-rename). `mcp.rs` → shim re-exporting prompt/resources/sse/tools + tools_query (2 external consumers); dropped 6 unused private re-exports (tool methods impl'''d on Server). lib.rs gained 11 `pub/pub(crate) mod mcp_*`. Response/RpcError fields → pub(crate) (siblings need access). include_str path fixed. Rewrites: top-level super::{parent items}→crate::mcp::{...}, sibling super::→crate::mcp_X, test-internal super:: kept. Build green, 1096 tests pass, guards 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flattened `ingest` subdir: 12 `src/ingest/*.rs` → `src/ingest_*.rs` at src/ root (prefix-rename, dodges config collision). `ingest/mod.rs` → `ingest.rs` shim re-exporting the 12 submodules (`pub use crate::ingest_config as config;` etc.) + items so `crate::ingest::X` still resolves (~18 consumers unchanged). lib.rs gained 12 `pub mod ingest_*`. Rewrites: `super::X`(sibling)→`crate::ingest_X`, `crate::ingest::X`(own submod)→`crate::ingest_X`, ITEM refs kept. Build green, 1096 tests pass, guards 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flattened `gnn` subdir: 13 `src/gnn/*.rs` → `src/gnn_*.rs` at src/ root (prefix-rename, dodges graph/persist collisions). `gnn/mod.rs` → `gnn.rs` shim re-exporting the 13 submodules (`pub use crate::gnn_gcn as gcn;` etc.) so `crate::gnn::X` still resolves (60 refs unchanged). lib.rs gained 13 `pub mod gnn_*`. Rewrites: `super::X`(sibling)→`crate::gnn_X`, `crate::gnn::X`(own submod)→`crate::gnn_X`, `crate::gnn::GnnError` kept. Build green, 1096 tests pass, guards 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — build-fix: retarget missed consumers `src/mcp/tools_mutate.rs` (5 refs) + `src/mcp/tools_admin.rs` (1 ref) from `crate::commands::graph_ops::`/`crate::commands::admin::` → `crate::commands_graph_ops::`/`crate::commands_admin::`. The commands flatten (2104fde) left these dangling; build was red (8 errors) for 2 commits. Build green, 1096 tests pass. Decided by: feb.
- 2026-08-06 — flattened `gossip` subdir: 13 `src/gossip/*.rs` → `src/gossip_*.rs` at src/ root (prefix-rename, dodges identity/types collisions). mod.rs deleted; lib.rs gained 13 `pub mod gossip_*`. Rewrites: `crate::gossip::X`→`crate::gossip_X`, `super::X` (sibling)→`crate::gossip_X`, grouped `use crate::gossip::{X,Y}`→split, test `use super::*` kept. External tools_delegate.rs+commands.rs retargeted. Build green, 11 test suites pass, guards 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flattened `commands` subdir: 11 `src/commands/*.rs` → `src/commands_*.rs` at src/ root (prefix-rename). Parent `src/commands/src/lib.rs` mod-block dropped; lib.rs gained the 11 module declarations. Sibling refs `super::route`→`crate::commands_route`, parent-item refs `super::{load_graph,...}`→`crate::commands::{...}`, `pub(super)`→`pub(crate)`. External `crate::commands::graph_ops::`→`crate::commands_graph_ops::`. Build green, tests pass, guards 0. Decided by: feb.
- 2026-08-07 — flatten `src/base/` into `src/` root (phase of full src/ flattening): 22 files. `src/base/{store,types,constants}.rs`→`src/base_{store,types,constants}.rs` (collided with existing root `store.rs`/`types.rs`); the other 19 kept bare names (`accept.rs`→`src/graph/src/accept.rs` etc). `src/base.rs` deleted; `src/lib.rs` declares the 22 modules at crate root (`pub mod`). Rewrites across 70 consumer files: `crate::base::store/types/constants`→`crate::base_store/base_types/base_constants`, `crate::base::X`→`crate::X`; subfile `super::store/types/constants`→`crate::base_store/base_types/base_constants`; `use crate::base_constants as constants;` alias where bare `constants::` used. Build clean, 1096 lib tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-07 — flatten `src/config/` into `src/` root (phase of full src/ flattening): `src/config/mod.rs`→`src/config/src/config.rs`, 17 subfiles→`src/config_*.rs` (config_embed.rs, config_gnn.rs, config_gossip.rs, config_graph.rs, config_hub.rs, config_ingest.rs, config_intake.rs, config_io.rs, config_preset.rs, config_reason.rs, config_reload.rs, config_retrieval.rs, config_secrets.rs, config_serve.rs, config_tick.rs, config_watcher.rs, config_detached_log.rs). Subfiles became crate-root modules declared in lib.rs (`mod` kept private, `pub mod` for detached_log + io — visibility preserved). config.rs re-exports retargeted to `crate::config_*::`; body `io::Error`→`crate::config_io::Error`. External: `crate::config::detached_log::stdio`→`crate::config_detached_log::stdio` (hub_node.rs, commands/mcp_cmd.rs); `kern::config::io::Error`→`kern::config_io::Error` (main.rs). Build clean, 1096 lib tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).

- 2026-08-06 — inlined the `transport` sub-crate into the root `kern` lib crate as `crate::transport`, completing the single-crate goal. `src/transport/` was a path-only workspace member (27 files, 4250 LoC, no external consumer); moved `src/transport/src/{lib,http,mcp}.rs` + `hub_rpc/`+`kern_rpc/`+`typed/`+`wire/` up to `src/transport/{mod,http,mcp,...}` (`lib.rs`→`mod.rs`), dropped its `Cargo.toml` + the `transport` path-dep + workspace member. Internal `crate::`→`crate::transport::` across moved files (avoids the collision between transport's `mod mcp` envelope and kern's root `pub mod mcp` server); `service!` invokers `crate::service!`→`crate::transport::service!`; removed `extern crate self as transport;`. The `transport-macros` proc-macro stayed its own crate (Rust forbids proc-macros in a lib); its `service!` codegen retargeted `::transport::`→`crate::transport::` (the `::kern::` self-ref did NOT resolve from proc-macro expansion — verified by failed build). 15 consumer files rewritten `transport::`→`crate::transport::`. Folded deps into root: `tokio-util`+codec, `bytes`, `futures`, unix `libc`, windows `windows-sys`. Build clean, 1096 lib tests pass, 59 transport tests pass, all test targets compile, guards exit 0, code-reviewer approved. `transport-macros` is the only remaining workspace member — a forced Rust exception, not actionable. Decided by: single-crate-fold (user-directed structural merge).

- 2026-08-06 — inlined the `watcher` sub-crate into the root `kern` lib crate as `crate::watcher`. `src/watcher/` was a path-only workspace member (no external consumer); folded its 6 files up to `src/watcher/{mod,event,ignore_rules,pipeline,file}.rs` (inner `watcher.rs`→`file.rs` to avoid clippy `module_inception`), dropped its `Cargo.toml` + the `watcher` path-dep + workspace member, folded its unique deps (`notify`,`ignore`) into root (the rest were already root deps), rewrote the two consumers (`src/ingest/file_watcher.rs`, `src/commands/src/lib.rs`) `use watcher::`→`use crate::watcher::`, fixed the two internal `use crate::event::`→`use super::event::`, and moved the integration test to `tests/watcher_tests.rs` (`use watcher::`→`use kern::watcher::`). One step in a 2-fire move to a single source crate; `transport` is the next fire. `transport-macros` stays its own crate — proc-macros cannot live in a lib crate. Build clean, 7/7 watcher tests pass, code-reviewer approved. Decided by: single-crate-fold (user-directed structural merge).
- 2026-08-06 — flatten `rpc` and `hub` subdirs into `src/` root (phase 1 of full src/ flattening): `src/rpc/`→`src/rpc.rs` + `src/rpc_kern_rpc_server.rs`; `src/hub/`→`src/hub/src/lib.rs` + `src/hub_node.rs` + `src/hub_serve.rs`. Submodule paths `crate::rpc::kern_rpc_server`→`crate::rpc_kern_rpc_server`, `crate::hub::node`→`crate::hub_node`; re-exports kept public API stable. Build clean, 36 tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flatten `tick` subdir (8 files) into `src/` root (phase 2 of full src/ flattening): `src/tick/{cluster,gnn_propagate,idle,pulse,queue,stigmergy,tasks,trainer}.rs`→`src/tick_*.rs`. Parent `src/tick_loop/src/tick.rs` kept; removed its `pub mod` block; sibling `use cluster::`→`use crate::tick_cluster::` etc; inline `trainer::Trainer`→`crate::tick_trainer::Trainer`, `stigmergy::run_gc`→`crate::tick_stigmergy::`, `idle::`→`crate::tick_idle::`. Subfiles: `use super::queue::`→`use crate::tick_queue::`, `use super::cluster::`→`use crate::tick_cluster::`. External consumers (~8 files): `crate::tick::queue::`→`crate::tick_queue::`, bare `tick::pulse::`→`crate::tick_pulse::`; removed unused `use crate::tick;`. Build clean, 1096 lib tests + 101 tick tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flatten `watcher` subdir (4 files + mod.rs) into `src/` root (phase 3 of full src/ flattening): `src/watcher/{event,file,ignore_rules,pipeline}.rs`→`src/watcher_*.rs`, `src/watcher/mod.rs`→`src/util/src/watcher.rs` (removed `mod` block, re-exports via `pub use crate::watcher_*`). Subfile `use super::event::`→`use crate::watcher_event::`, `use super::ignore_rules::`→`use crate::watcher_ignore_rules::`. External consumers unchanged (use top-level `crate::watcher::` re-exports only). Build clean, 7 watcher_tests + 37 lib tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).

- 2026-08-06 — deleted `pub fn is_semantic` from `ReasonKind` (`src/base/types.rs`). The predicate (`matches! Similarity | Provenance | Ratification`) had zero callers — `rg 'is_semantic' src/` returns nothing; the enum variants stay live (Similarity used in tick/accept/commands/query). A dead `pub` predicate is surface area with no consumer; if the classification is ever needed it's a one-line `matches!` at the call site. Also reconciled `docs/ideas.md`: the stale open copies of B6/B3/B4/B1 (all already closed) were pruned and B6 got its `## Closed` entry. Net -99 lines of dead doc.

- 2026-08-06 — replaced the dead oracle pre-commit hook with the windmill
  gate. The tracked `hooks/pre-commit` (oracle ruling) targeted `docs/windmill/`
  (or root `CHANGELOG.md`), neither of which exists; the repo lives under
  `docs/windmill/`, so the hook matched nothing and let every commit through.
  The windmill gate targets `docs/windmill/{CHANGELOG,VISION,FEATURES,ROADMAP}.md`
  — the real layout — so rule 1 fires again. No backup kept (the oracle hook
  was inert: wrong path + no `CLAUDE.local.md` means its citation check never
  ran). `core.hooksPath=hooks` unchanged; the tracked file IS the live hook.
  Decided by: the oracle

- 2026-08-06 — removed `legacy_network_id` from `ParamsV0` + `legacy_contract`.
  Alpha has no compat (AGENTS.md): no legacy decode fallbacks, no migration
  paths. The `legacy_network_id: Option<String>` field on `ParamsV0` and the
  `legacy_contract()` helper were migration shims for old `network_id`-mode
  clusters. Removed wholesale from `contract.rs`, `privacy.rs`, `handler.rs`.
  `FORMAT_VERSION` 9 → 10 (old stores rejected, never migrated). Docs updated.

- 2026-08-02 — the `query` tool's `inputSchema` no longer carries a top-level
  `anyOf`. The Anthropic tool API rejects `anyOf`/`oneOf`/`allOf` at the root of
  `input_schema`, so **every** request from a host that advertised the kern tool
  set failed with `400 invalid_request_error: tools.N.custom.input_schema:
  input_schema does not support oneOf, allOf, or anyOf at the top level` — one
  bad schema took down the whole session, across every upstream provider. The
  "at least one of `text`, `id`, `ids`" rule was always enforced twice; it now
  lives only where it works: the tool description and `tool_query`'s runtime
  guard (`either text, id or ids is required`). No behaviour change for callers.

  **Decided by:** the schema was documentation for a constraint the runtime
  already owned, so deleting it costs nothing. A new test
  (`no_tool_schema_carries_a_root_combinator`) walks every entry of
  `tool_definitions()` and fails on any root combinator, so no future tool can
  reintroduce the outage; the old test that *asserted* the `anyOf` existed was
  inverted into that guarantee.

- 2026-08-02 — `kern ingest --file` resolves a relative path against the
  directory the caller invoked kern from, not the project root `main` re-pins
  cwd to. The re-pin stays (a subdir launch must not boot an empty graph) but it
  applied to caller-supplied paths too, so `cd sub && kern ingest --file b.md`
  raised a bare ENOENT — and, when a same-named file sat at the project root,
  **silently ingested the root file instead**, returning a success line for
  content the caller never passed. New `set_launch_dir` / `launch_dir_join`
  (`src/lib.rs`): the pre-pin cwd is recorded in a `OnceLock` before dispatch,
  absolute paths pass through, and no recorded dir keeps the old behaviour for
  library embedders. Error text names the resolved path. `--file` was the only
  flag reading a caller path after the re-pin.

  **Decided by:** verify-before-claiming — both failure modes reproduced
  against the release binary before and after, full suite 1018 tests green,
  three unit tests pin the resolution branches. Surfaced by an agent-side
  batch writer that had been failing closed on this for weeks, which is why a
  per-item `kern ingest` subprocess storm was never replaced by the batch path.

- 2026-07-24 — item 93 tax paid again: `FEATURES.md` drifted line anchors
  re-pointed to current source. The live inventory had accumulated 56 anchor
  nominations — citations naming a symbol (`traversal_count`, `observe_lamport`,
  `spawn_child_clusters`, the `do_*` tick tasks, `struct Reason`/`Kern`,
  `HealthStats`, `with_timeout_secs`) but pointing at a bare `}`, a blank line or
  unrelated code, because both the doc and the source grew and the line numbers
  never followed. Twenty-two were re-anchored to the verified definition line
  (resolved by pairing each citation with the symbol its sentence names, then
  locating that symbol's unique `fn`/`struct`/field definition in the cited
  file); the lamport pair `` `:443` ``/`` `:450` `` was hand-corrected to
  bump=467 / observe=474 rather than trusting nearest-left pairing, and two
  auto-resolutions were dropped as unsafe — a generic `from` that resolved into a
  test body, and one mispaired lamport anchor. `FEATURES.md` nominations fell
  56 → 34; `python3 tests/docs_check.py` still exits 0 (nominations never gated).
  The 34 residual are the dense §16 LLM-client continuations and precise
  struct-offset pointers, where hand-chasing is error-prone and low-value.

  **This is the tax, not the fix.** Item 93's symbolic anchors
  (`` `FEATURES.md#16-llm-client` ``, immune to insertion) remain the real
  answer and stay open; every reconcile pass that re-points line numbers is
  paying interest on that debt. The nomination count grew 39 → 56 in
  `FEATURES.md` alone since item 93 was written 2026-07-21, which is the debt
  compounding on schedule.

  **Decided by:** fix-the-root (name the residual as the symbolic-anchor debt it
  is, don't pretend line-chasing closes item 93), verify-before-claiming (each
  re-anchor confirmed against the target file's actual definition line, unsafe
  auto-resolutions dropped, nomination drop measured 56 → 34 not asserted).

- 2026-07-24 — item 93 residual: `docs_check.py` is green again, and the
  illustration escape now covers every citation form. `docs_check.py` had been
  red since 2026-07-22 with five `beyond EOF` dead references, all false
  positives. Root cause: in `check_page`, the `ILLUSTRATION` regex blanks
  double-backtick spans into `quoted`, but only `BARE_RS` and `CONTINUATION` ran
  over `quoted` — `REF`, `REPO_PATH` and `SIBLING_REF` still ran over raw `text`.
  So the escape was form-dependent: a bare `:NNN` inside `` `` `` was silent, but
  an illustrated spelled-out path `` `src/llm/src/llm.rs:11434` `` was matched as a
  phantom past-EOF citation. The item 93 fenced-block pass had claimed "After: 0
  dead references", which was false on the real tree — those three tokens are
  inline illustrations, not fenced blocks, so the fence skip never reached them.
  Fixed by running every citation form over `quoted`, so the double-backtick
  escape is uniform; the four surviving single-backtick port tokens
  (`:11434`/`:8080` in `FEATURES.md` and `ROADMAP.md`) were converted to the
  documented `` `:11434` `` illustration idiom. A new selftest fixture pins both
  directions: an illustrated full path past EOF inside double backticks is
  silent, and the same token without the double backticks reds as `beyond EOF`.
  `python3 tests/docs_check.py` now exits 0 and `--selftest` prints `selftest
  OK`. Decided by: fix-the-root (the escape is made uniform across all matchers,
  not the five citations edited one by one), verify-before-claiming (the
  negative control reds, and the false "0 dead references" claim was measured
  wrong before it was corrected).

- 2026-07-23 — item 103 closed: the LongMemEval-S run is recorded, completing
  the public-benchmark pair. Seeded 100-question sample (seed 13, runner
  default) of LongMemEval-S, `qwen3-embedding:0.6b`, direct path, k=10, 4792
  sessions ingested; report `tests/eval/reports/longmemeval-20260723-193342.json`.
  Session granularity: any@1 0.83 / any@5 0.97 / any@10 0.99 / MRR 0.8896.
  Turn granularity (93 labeled): any@5 0.8065 / any@10 0.9032 / MRR 0.6271.
  Weakest type: single-session-preference (any@1 0.429, n=7). Latency p50
  0.53s / p95 0.63s cold-process CLI. Honesty: sample not full set (`--full`
  exists); binary rebuilt mid-run with a `kern get`-only change the query path
  never executes; LongMemEval's published LLM-judged accuracies are not
  comparable to retrieval-only ranks. Decided by: verify-before-claiming (the
  number is recorded with its protocol and caveats, no stronger sentence
  licensed), name-the-tradeoff (100-sample now over full-set hours — the seed
  is pinned so the sample is reproducible and extendable).

- 2026-07-23 — item 104 ground half: kern gets its own committed ground-truth
  corpus and the first distill-path benchmark numbers. `tests/e2e/eval/ground.json`
  (self-authored CC0, 8 sessions / 82 turns / 34 questions, evidence cited as
  `[session, 1-based turn, anchor substring]`, anchors enforced by
  `tests/e2e/test_eval_ground.py` in CI); `run_ground.py` (+ `just eval-ground`)
  scores the same turn-level labels over two paths: `direct` (documents, the
  verbatim floor — the LoCoMo protocol) and `distill` (`.kern/intake/` + `kern
  intake drain`, the real pipeline; retrieved claims map back to cited turns via
  `kern get`'s new `Source:` line — `entity_detail` now carries
  `source.{scheme,object_id,section}`, pinned in
  `detail_json_carries_everything_the_get_printer_needs`). First numbers
  (qwen3-embedding:0.6b, distill qwen3.5:4b, k=10, report
  `ground-20260723-192745.json` + direct in `ground-20260723-190855`-series):
  direct recall_any@10 **0.824** / MRR 0.407; distill recall_any@10 **0.324** /
  MRR 0.259. The gap is ingest coverage, not retrieval: only 24 distinct claims
  were ever retrieved for 82 turns, and 17% of delivered hits cite no turns —
  the distiller compresses away what single-hop questions need (0.20 any@10,
  worst category; temporal/update ≈ 0.43–0.60 because dated facts survive
  distillation). Fixed en route: `fake_llm.distilled` never matched the `[i]`
  turn markers the provenance prompt added 2026-07-22, so every fake-LLM
  distill quietly produced zero claims; it now strips the marker and cites the
  turn. Harness `write_config` gains a `reason=(url, model)` knob (byte-identity
  default). Decided by: verify-before-claiming (the pipeline's number measured
  before any tuning; the finding is distill coverage, recorded not guessed),
  name-the-tradeoff (a small self-authored corpus is self-graded and easy —
  accepted as the committed, license-free, CI-validatable baseline the NC-licensed
  datasets cannot be; cross-model claims still belong to LoCoMo/LongMemEval),
  avoided-question-first (the LoCoMo daemon-mode bench stays open — this is the
  same shape on the corpus we may redistribute).

- 2026-07-23 — item 48 measurement half-closed: the active ingest dedup config
  (global `dedup_threshold` + per-kind `dedup_threshold_by_kind`) is now
  surfaced. `Server::health_stats` (`src/mcp/src/lib.rs`) JSON `ingest:` block;
  `trnsprt::HealthRes` gains `#[serde(default)] ingest_dedup_threshold` +
  `ingest_dedup_threshold_by_kind` (old daemon → `0.0`/`[None;5]`); `kern health`
  prints `dedup:` daemon-sourced only; `kern://local/health` by construction.
  Proved by `kern_health_prints_dedup_config` + dto round-trip + old-payload
  absence guard. `cargo test -p kern --lib` 965 passed, 0 failed, 4 ignored;
  `cargo test -p trnsprt --lib` 61 passed.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-23 — item 66 measurement completion: the four remaining
  `RetrievalConfig` knobs (`seed_k`, `mmr_enabled`, `lexical_enabled`,
  `pagerank_enabled`) now surface in the existing `retrieval:` JSON block +
  `HealthRes` + `kern health` (one line `seed_k N, mmr {bool}, lexical {bool},
  pagerank {bool}`, daemon-sourced) + RPC map. `kern_health_prints_retrieval_config`
  extended (5 lines); dto round-trip `seed_k 30/mmr false/lexical true/pagerank
  true`; old-payload-absence → `0/false` (standing guard). `cargo test -p kern
  --lib` 964 passed, 0 failed, 4 ignored; `cargo test -p trnsprt --lib` 61
  passed. Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-23 — item 20 measurement half-closed: the active `source_trust` map
  (`RetrievalConfig.source_trust`, `BTreeMap` keyed on `Source::scheme()`,
  empty by default = bit-identical scoring) is now surfaced. `Server::health_stats`
  (`src/mcp/src/lib.rs`) JSON carries `source_trust`; `trnsprt::HealthRes` gains
  `#[serde(default)] source_trust: BTreeMap<String, f64>` (old daemon → empty);
  `kern health` prints `source-trust:` daemon-sourced only (item 100 rule),
  empty → `(none)`, no daemon → no line; `kern://local/health` by construction.
  Proved by `kern_health_prints_source_trust` + dto round-trip + old-payload
  absence → empty (standing guard). `cargo test -p kern --lib` 964 passed, 0
  failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-23 — item 66 measurement half-closed: the active RRF config
  (`rrf_k`, `rrf_global_weight`, the three `ModeWeights`
  `weights_content`/`weights_reason`/`weights_hybrid`) is now surfaced.
  `Server::health_stats` (`src/mcp/src/lib.rs`) JSON carries a `retrieval:` block from
  `self.cfg.retrieval`; `trnsprt::HealthRes` gains a nested `RetrievalHealth`
  `#[serde(default)]` (old daemon → zeroed); `kern health` prints 4 lines
  (header + content/reason/hybrid weights) daemon-sourced only; `kern://local/health`
  by construction. Proved by `kern_health_prints_retrieval_config` +
  `every_health_field_round_trips_through_json` (retrieval round-trip).
  `cargo test -p kern --lib` 961 passed (1 pre-existing flake); `cargo test -p
  trnsprt --lib` 61 passed. Standing guard: old-payload-absence → zeroed.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.
  Still open: the tuning sweep — RRF weights/blends never measured vs recall.

- 2026-07-23 — item 83 Gini-over-kern-sizes gauge: `gini_over_kern_sizes(counts:
  &[usize]) -> f64` (new, `src/base/health.rs`, finite-n max `(n−1)/n`) +
  `HealthStats.gini_kern_sizes` (filled from the resident-kern walk) — the
  distribution the `largest_kern_entities` max summarises. `kern health` `kerns:`
  line gains `gini N.NN`; MCP `health` JSON + `HealthRes` `#[serde(default)]`
  (old daemon → `0.0`), daemon-sourced only. Proved by
  `gini_over_kern_sizes_pins_known_distributions` (`[10,0,0]` → `2/3`, `[100,0]`
  → `1/2`), `graph_health_stats_reports_gini_kern_sizes` (10 + four empty →
  `4/5`), dto round-trip `0.42`. `cargo test -p kern --lib` 961 passed (1
  pre-existing flake); `cargo test -p trnsprt --lib` 61 passed. Negative control
  (`→ 0.0` reds) green on revert.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-23 — item 25 guard added: `non_access_mutations_leave_mutation_epoch_unchanged`
  (`src/retrieval/seed.rs`) pins `g.mutation_epoch()` unchanged across the four
  named non-access sites (`merge_remote_entity`, reembed `values_mut`, gossip
  `inject_remote_scope`/`new_phantom_kern`, `do_cluster` `move_entity`) —
  companion to `an_eligibility_change_is_reflected_with_no_epoch_bump` (access
  site). Same item-77 shape. Negative control (add a bump at one site → sub-
  assert reds, `right: 0` epoch 0→1, green on revert). `cargo test -p kern --lib`
  959 passed, 0 failed, 4 ignored. Decided by: fix-the-root, name-the-tradeoff,
  verify-before-claiming.

- 2026-07-23 — item 55 measurement half-closed: the QBST recency half-life
- 2026-07-23 — multi-tenancy scoping: user_id/agent_id/session_id on Entity, threaded through ingest + query filter (FORMAT_VERSION 7→8)
  (`qbst_recency_half_life_secs`, `src/config/retrieval.rs`, 24h default) is now
  surfaced — companion to the item 62 heat line. `HealthRes.qbst_recency_half_life_secs`
  `#[serde(default)]` (old daemon → `0`); `Server::health_stats` JSON line;
  `kern health` `recency: half-life {N}s` daemon-sourced only (item 100 rule);
  `kern://local/health` by construction. Proved by `kern_health_prints_heat_half_life`
  (extended: both `recency: half-life 0s` + `86400s`), dto round-trip `86400` +
  old-payload → `0`. `cargo test -p kern --lib` 957 passed (1 pre-existing
  `the_sink_waits` flake, green isolated); `cargo test -p trnsprt --lib` 61
  passed. Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.
  Still open: the tuning sweep (item 55/87) — neither half-life measured vs
  recall.

- 2026-07-23 — item 48 beside half-closed (per-kind dedup threshold, default-off):
  `IngestConfig.dedup_threshold_by_kind: [Option<f64>; 5]` (new, indexed by
  `EntityKind as u8`, default `[None; 5]` = bit-identical) + `dedup_threshold_for(kind)`
  resolver (`None` → global). `validate` rejects out-of-range `Some` naming the
  kind. Three production call sites (`place.rs` `place_document`/`place_chunks`,
  `worker.rs`) resolve per-kind; `ingest_cmd` + `mcp tools_mutate` bridge into
  the runtime `Config`. Array indexed by `as u8` avoids adding `Hash` to
  `EntityKind`. Proved by `dedup_threshold_for_kind_resolves`,
  `validate_rejects_out_of_range_per_kind`,
  `per_kind_dedup_threshold_tightens_facts_loosens_claims` (Fact `Some(0.99)`
  keeps `0.97`; Claim `Some(0.80)` dedups `0.81`). Existing dedup/place green
  unedited at default. `cargo test -p kern --lib` 958 passed, 0 failed, 4
  ignored; `cargo test -p trnsprt --lib` 61 passed. Negative control (Fact slot
  → `None` → `0.97` dedups → reds) green on revert.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.
  Still open: hard paraphrase-evadable dedup key (main body).

- 2026-07-23 — item 62 `kern://health` surfacing closed: the active heat
  retention half-life (`HeatConfig.half_life_secs`, the one `Preset::apply`
  sets — relaxed=30d / medium=7d / tight=3d) is now surfaced. `Server::health_stats`
  (`src/mcp/src/lib.rs`) JSON carries `heat_half_life_secs` from `self.cfg.heat`;
  `trnsprt::HealthRes` gains `#[serde(default)] heat_half_life_secs` (old daemon
  → `0`); `kern health` prints `heat: half-life {N}s` daemon-sourced only (item
  100 rule); `kern://local/health` carries it by construction. Proved by dto
  round-trip `2592000` + old-payload absence → `0`, and
  `kern_health_prints_heat_half_life` (30d → `2592000s`, `0` → `0s`, no daemon →
  no line); negative control (omit field → `0` → print reds, green on revert).
  `cargo test -p kern --lib` 955 passed, 0 failed, 4 ignored; `cargo test -p
  trnsprt --lib` 61 passed. Decided by: fix-the-root, name-the-tradeoff,
  verify-before-claiming. Still open: top-10 stability; item 54 GC gate.

- 2026-08-07 — split the single `kern` crate into a 24-member workspace of concept crates (no `kern-` prefix, llm/src shape): util, base, math, store_core, store, bootstrap, graph, ingest_config, llm, config, gnn, retrieval, ingest, tick, gossip, tick_loop, transport, test_support, health, mcp, rpc, hub, commands, plus the `kern` binary. Each crate has Cargo.toml + README + its own `src/lib.rs`. Cycle-breaks by moving pure helpers into lower crates: `entity_detail_by_id`/`base_entity_json`→retrieval::id_detail; `link_entities`/`forget_entity`/`promote_entity`/`forget_by_source`/`degrade_entity_reasons`→graph::graph_ops; `graviton_rows`→graph::graph_ops; `load_graph`/`save_graph_guarded`/`snapshot_if_dirty`/`reconcile_if_stale`/`bind_embed_model`/`apply_graph_config`/`reload_graph`→bootstrap; `store::base_store`/`store::lock`→store_core (split out so graph→store_core stays acyclic with store::Registry needing them); `launch_dir_join`/`set_launch_dir`→commands. `kern` lib.rs reduced to 25 lines (just `pub use` re-exports). 1108 tests pass, `cargo clippy --all-targets --workspace` clean. Decided by: continue-folding (user-directed structural split, llm/src shape).
