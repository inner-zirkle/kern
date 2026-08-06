# Changelog

<!-- docs-check: historical -->

- 2026-08-07 — flatten `src/base/` into `src/` root (phase of full src/ flattening): 22 files. `src/base/{store,types,constants}.rs`→`src/base_{store,types,constants}.rs` (collided with existing root `store.rs`/`types.rs`); the other 19 kept bare names (`accept.rs`→`src/accept.rs` etc). `src/base.rs` deleted; `src/lib.rs` declares the 22 modules at crate root (`pub mod`). Rewrites across 70 consumer files: `crate::base::store/types/constants`→`crate::base_store/base_types/base_constants`, `crate::base::X`→`crate::X`; subfile `super::store/types/constants`→`crate::base_store/base_types/base_constants`; `use crate::base_constants as constants;` alias where bare `constants::` used. Build clean, 1096 lib tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-07 — flatten `src/config/` into `src/` root (phase of full src/ flattening): `src/config/mod.rs`→`src/config.rs`, 17 subfiles→`src/config_*.rs` (config_embed.rs, config_gnn.rs, config_gossip.rs, config_graph.rs, config_hub.rs, config_ingest.rs, config_intake.rs, config_io.rs, config_preset.rs, config_reason.rs, config_reload.rs, config_retrieval.rs, config_secrets.rs, config_serve.rs, config_tick.rs, config_watcher.rs, config_detached_log.rs). Subfiles became crate-root modules declared in lib.rs (`mod` kept private, `pub mod` for detached_log + io — visibility preserved). config.rs re-exports retargeted to `crate::config_*::`; body `io::Error`→`crate::config_io::Error`. External: `crate::config::detached_log::stdio`→`crate::config_detached_log::stdio` (hub_node.rs, commands/mcp_cmd.rs); `kern::config::io::Error`→`kern::config_io::Error` (main.rs). Build clean, 1096 lib tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).

- 2026-08-06 — inlined the `transport` sub-crate into the root `kern` lib crate as `crate::transport`, completing the single-crate goal. `src/transport/` was a path-only workspace member (27 files, 4250 LoC, no external consumer); moved `src/transport/src/{lib,http,mcp}.rs` + `hub_rpc/`+`kern_rpc/`+`typed/`+`wire/` up to `src/transport/{mod,http,mcp,...}` (`lib.rs`→`mod.rs`), dropped its `Cargo.toml` + the `transport` path-dep + workspace member. Internal `crate::`→`crate::transport::` across moved files (avoids the collision between transport's `mod mcp` envelope and kern's root `pub mod mcp` server); `service!` invokers `crate::service!`→`crate::transport::service!`; removed `extern crate self as transport;`. The `transport-macros` proc-macro stayed its own crate (Rust forbids proc-macros in a lib); its `service!` codegen retargeted `::transport::`→`crate::transport::` (the `::kern::` self-ref did NOT resolve from proc-macro expansion — verified by failed build). 15 consumer files rewritten `transport::`→`crate::transport::`. Folded deps into root: `tokio-util`+codec, `bytes`, `futures`, unix `libc`, windows `windows-sys`. Build clean, 1096 lib tests pass, 59 transport tests pass, all test targets compile, guards exit 0, code-reviewer approved. `transport-macros` is the only remaining workspace member — a forced Rust exception, not actionable. Decided by: single-crate-fold (user-directed structural merge).

- 2026-08-06 — inlined the `watcher` sub-crate into the root `kern` lib crate as `crate::watcher`. `src/watcher/` was a path-only workspace member (no external consumer); folded its 6 files up to `src/watcher/{mod,event,ignore_rules,pipeline,file}.rs` (inner `watcher.rs`→`file.rs` to avoid clippy `module_inception`), dropped its `Cargo.toml` + the `watcher` path-dep + workspace member, folded its unique deps (`notify`,`ignore`) into root (the rest were already root deps), rewrote the two consumers (`src/ingest/file_watcher.rs`, `src/commands.rs`) `use watcher::`→`use crate::watcher::`, fixed the two internal `use crate::event::`→`use super::event::`, and moved the integration test to `tests/watcher_tests.rs` (`use watcher::`→`use kern::watcher::`). One step in a 2-fire move to a single source crate; `transport` is the next fire. `transport-macros` stays its own crate — proc-macros cannot live in a lib crate. Build clean, 7/7 watcher tests pass, code-reviewer approved. Decided by: single-crate-fold (user-directed structural merge).
- 2026-08-06 — flatten `rpc` and `hub` subdirs into `src/` root (phase 1 of full src/ flattening): `src/rpc/`→`src/rpc.rs` + `src/rpc_kern_rpc_server.rs`; `src/hub/`→`src/hub.rs` + `src/hub_node.rs` + `src/hub_serve.rs`. Submodule paths `crate::rpc::kern_rpc_server`→`crate::rpc_kern_rpc_server`, `crate::hub::node`→`crate::hub_node`; re-exports kept public API stable. Build clean, 36 tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flatten `tick` subdir (8 files) into `src/` root (phase 2 of full src/ flattening): `src/tick/{cluster,gnn_propagate,idle,pulse,queue,stigmergy,tasks,trainer}.rs`→`src/tick_*.rs`. Parent `src/tick.rs` kept; removed its `pub mod` block; sibling `use cluster::`→`use crate::tick_cluster::` etc; inline `trainer::Trainer`→`crate::tick_trainer::Trainer`, `stigmergy::run_gc`→`crate::tick_stigmergy::`, `idle::`→`crate::tick_idle::`. Subfiles: `use super::queue::`→`use crate::tick_queue::`, `use super::cluster::`→`use crate::tick_cluster::`. External consumers (~8 files): `crate::tick::queue::`→`crate::tick_queue::`, bare `tick::pulse::`→`crate::tick_pulse::`; removed unused `use crate::tick;`. Build clean, 1096 lib tests + 101 tick tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).
- 2026-08-06 — flatten `watcher` subdir (4 files + mod.rs) into `src/` root (phase 3 of full src/ flattening): `src/watcher/{event,file,ignore_rules,pipeline}.rs`→`src/watcher_*.rs`, `src/watcher/mod.rs`→`src/watcher.rs` (removed `mod` block, re-exports via `pub use crate::watcher_*`). Subfile `use super::event::`→`use crate::watcher_event::`, `use super::ignore_rules::`→`use crate::watcher_ignore_rules::`. External consumers unchanged (use top-level `crate::watcher::` re-exports only). Build clean, 7 watcher_tests + 37 lib tests pass, guards exit 0. Decided by: single-crate-fold (user-directed full src/ flattening).

- 2026-08-06 — deleted `pub fn is_semantic` from `ReasonKind` (`src/base/types.rs`). The predicate (`matches! Similarity | Provenance | Ratification`) had zero callers — `rg 'is_semantic' src/` returns nothing; the enum variants stay live (Similarity used in tick/accept/commands/query). A dead `pub` predicate is surface area with no consumer; if the classification is ever needed it's a one-line `matches!` at the call site. Also reconciled `docs/ideas.md`: the stale open copies of B6/B3/B4/B1 (all already closed) were pruned and B6 got its `## Closed` entry. Net -99 lines of dead doc.

- 2026-08-06 — replaced the dead oracle pre-commit hook with the windmill
  gate. The tracked `hooks/pre-commit` (oracle ruling) targeted `docs/oracle/`
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

- 2026-07-27 — v1.3.0 cut. Workspace version 1.2.0 → 1.3.0 (`Cargo.toml`,
  `Cargo.lock`, `FEATURES.md` header/footer): the surface grew since v1.2.0 —
  federation plan v0 (signed envelopes on every gossip frame, ring routing,
  contract kerns with five new wire kinds, the `sign` and `contract_grant`
  mcp tools — eighteen tools now — and `[gossip]` config keys `ring`,
  `identity_path`, `sync_interval_secs`, `subscriptions`,
  `[[gossip.contracts]]`), plus the claim-kind `subClassOf` hierarchy
  (`claim_kind_parents`, closed-world, query closure over parents).
  Semver-minor; wire format changed (envelope) under alpha rules — peers
  upgrade together, no shims. Deps: ed25519-dalek 3, chacha20poly1305 0.11,
  toml 1; bincode held at 2 (persisted format) and heed at 0.20 (0.22 breaks
  the external-commit reconcile test) — holds commented in `Cargo.toml`.

  **Decided by:** verify-before-claiming (cut only after the full suite —
  1020 tests — ran green on the exact tree being installed), supersedes the
  v1.2.0 cut entry.

- 2026-07-27 — Federation plan v0 implemented (`docs/plans/FEDERATION_PLAN.md`
  §1–§6, all six phase gates green). ed25519 peer identity + signed wire
  envelopes (`src/gossip/identity.rs`; verify precedes every per-peer state
  touch, invalid frames counted); Kleinberg small-world ring with greedy
  routing (`ring.rs`; 1k-peer sim reaches nearest in ≤ log²n hops for 99%,
  survives 20% churn); contract-keyed shared kerns where
  `ContractId = blake3(policy || params)` and `merge.rs` is the default
  contract's `apply` (`contract.rs`; property-tested commutative/idempotent,
  summary/diff converges byte-identical in one exchange each way);
  subscription trees + anti-entropy (`subs.rs`, `handler.rs`; three-node
  real-socket gate, partition heals in one sync pass); daemon-as-delegate
  `sign`/`contract_grant` mcp tools (key never crosses the socket; grants
  move the ContractId and mint the tombstone signature); xchacha20poly1305
  private kerns (`privacy.rs`; relay-never-sees-plaintext proven by grepping
  the relay's serialized kern for the sentinel). Question rate budget re-keyed
  from spoofable `origin` to verified PeerId. Two tree-cycle guards added
  after fuzzing the 3-node gate: first-parent-wins on SubAck, and a
  downstream peer is never adopted as upstream. Deviations from the spec,
  recorded in the plan: iterative (requester-driven) ring join instead of
  recursive forwarding; `Summary` carries its `(id, lamport)` entries beside
  the bucket hashes (one round trip instead of per-bucket fetches); the
  signed-body cache is in-memory in v0. New deps: `ed25519-dalek`, `blake3`,
  `chacha20poly1305` — all license-clean for MIT; no Freenet code read or
  copied (clean-room, per the plan's legal note).

  **Decided by:** fix-the-root (the SubAck race was fixed in the protocol
  guards, not papered over in the test), verify-before-claiming (every phase
  ships its plan-named gate: sim, property, socket-e2e, sentinel-grep —
  1008 lib tests green), supersedes the 2026-07-26 federation direction
  entry's "spec only" status.

- 2026-07-25 — v1.2.0 cut. Workspace version 1.1.0 → 1.2.0 (`Cargo.toml`,
  `Cargo.lock`, `FEATURES.md` header/footer): the surface grew since v1.1.0 —
  the sixteenth tool `events` (the ctrl-Watcher change feed) and the
  `kern mcp --embed-url/--embed-model` per-process override — semver-minor,
  tagged `v1.2.0` so `release.yml` publishes the 15-target build. Checked and
  deliberately NOT done: prefixing served tool names with `mcp__` — the
  `mcp__kern__<tool>` spelling agents see is minted client-side from the
  `.mcp.json` `mcpServers.kern` key (`ensure_mcp_registered`,
  `src/commands/mcp_cmd.rs`); the wire names stay bare (`query`, `events`, …,
  pinned by `definitions_are_well_formed_and_complete`), and renaming them
  server-side would double-prefix every client to `mcp__kern__mcp__query`.

  **Decided by:** verify-before-claiming (prefix mechanism read from the
  registration code and the client convention, not assumed from the tool
  list), fix-the-root (the version records the released surface; the prefix
  "fix" was refused because the root already provides it).

- 2026-07-25 — repo-state audit reconciled the inventory and site to the tree.
  A full-feature audit (per-subsystem state, gaps, recorded numbers) found four
  drifts, all the class the 2026-07-24 site pass named — new surface outrunning
  the docs. (1) `tools/list` serves **sixteen** tools: `events`, the read-only
  change feed a ctrl Watcher polls (`src/mcp/tools_events.rs`, pinned second in
  `definitions_are_well_formed_and_complete`), landed after that pass, so
  `FEATURES.md` §12 said 15 and the site (`howto/mcp.mdx`, `content/llms.md`)
  still said fifteen — count, row and bullet added in all three. (2) `kern mcp
  --embed-url/--embed-model` (`EmbedArgs::apply_to`, `src/commands.rs`), the
  per-process embed override for container-spawned proxies, was recorded
  nowhere — added to `FEATURES.md` §14. (3) `howto/mcp.mdx` still warned that
  proxy mode answers `resources/list`/`prompts/list` with `-32601`, a claim
  item 81 closed 2026-07-22 — the graphless methods dispatch through the one
  `handle_graphless_method` on both paths, proven by
  `every_capability_the_proxy_advertises_is_answered_over_the_stdio_loop`; the
  callout now states what runs. (4) The `FEATURES.md` header's size stamp had
  drifted to fiction: 174 tracked `.rs` files at ~59.1k raw lines against the
  stated "~42.4k across 156" — restated with its measurement method so the next
  drift is checkable. The audit's quality findings needed no doc change: every
  number it surfaced (LoCoMo-10, LongMemEval-S, the ground distill-vs-direct
  gap, the scale tables) is already recorded under items 103/104 and the
  closed-items ledger. Beside the four, the audit found `docs-check` red on the
  tree: `732b87b` renamed `src/trnsprt/` → `src/transport/` and no doc
  followed — 54 dead references across `FEATURES.md` §13/§18 and `ROADMAP.md`,
  plus one report citation spelled `tests/eval/reports/` for an uncommitted
  artifact its sibling LoCoMo line spells `eval/reports/`. Paths rewritten to
  the live tree (this file's own historical entries keep the old spelling —
  they were true when recorded); `python3 tests/docs_check.py` exits 0 again.

  **Decided by:** verify-before-claiming (tool set taken from
  `tool_definitions()` and the test's expected array, proxy behavior from the
  passing test, sizes from `git ls-files | xargs wc -l` — never from the docs'
  own counts), fix-the-root (the missing tool and flags documented and the
  false proxy claim corrected, not just numbers bumped).

- 2026-07-24 — published docs reconciled to the live tool set. `tools/list`
  serves fifteen (`query`, `ingest`, `link`, `forget`, `forget_by_source`,
  `degrade`, `move`, `promote`, `health`, `graviton`, `claim_kind`, `pulse`,
  `gc`, `intake_drain`, `setup` — pinned by `definitions_are_well_formed_and_complete`
  in `src/mcp/tools.rs`), but the site still said thirteen and enumerated only
  thirteen: `forget_by_source` (host-deletion cascade, item 19) and `promote`
  (review-lifecycle release, item 21) landed after the last site pass and were
  invisible to any agent reading the docs. Fixed `howto/mcp.mdx` (count at the
  top and the verify step, plus a bullet for each missing tool) and
  `content/llms.md` (count and list). `next build` green (30 static routes),
  `docs_check` still exit 0. The published page is the one an agent wires
  against, so a missing tool there is a capability the user never learns exists.

  **Decided by:** verify-before-claiming (tool set taken from the source test's
  expected array, not the doc's own count; site rebuilt to confirm), fix-the-root
  (the drift is new tools outrunning the last site pass — added them, not just
  bumped the number).

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
  an illustrated spelled-out path `` `src/llm.rs:11434` `` was matched as a
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
  surfaced. `Server::health_stats` (`src/mcp.rs`) JSON `ingest:` block;
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
  (`src/mcp.rs`) JSON carries `source_trust`; `trnsprt::HealthRes` gains
  `#[serde(default)] source_trust: BTreeMap<String, f64>` (old daemon → empty);
  `kern health` prints `source-trust:` daemon-sourced only (item 100 rule),
  empty → `(none)`, no daemon → no line; `kern://local/health` by construction.
  Proved by `kern_health_prints_source_trust` + dto round-trip + old-payload
  absence → empty (standing guard). `cargo test -p kern --lib` 964 passed, 0
  failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-23 — item 87 measurement half-closed: the active preset name
  (`relaxed`/`medium`/`tight`) is now surfaced. `Server::health_stats`
  (`src/mcp.rs`) JSON carries `preset` from `self.cfg.preset`; `trnsprt::HealthRes`
  gains `preset: String` `#[serde(default)]` (old daemon → `""`); `kern health`
  prints `preset: {name}` daemon-sourced only (item 100 rule), first line framing
  the heat/recency/retrieval lines; `kern://local/health` by construction.
  Proved by `kern_health_prints_preset` (tight/relaxed/empty/no-daemon) + dto
  round-trip `preset: "tight"`. Standing guard: old-payload absence → `""`
  (the `tight` print reds if omitted). `cargo test -p kern --lib` 963 passed, 0
  failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.
  Still open: the tuning sweep — run the suite per preset, re-pin the baseline.

- 2026-07-23 — item 66 measurement half-closed: the active RRF config
  (`rrf_k`, `rrf_global_weight`, the three `ModeWeights`
  `weights_content`/`weights_reason`/`weights_hybrid`) is now surfaced.
  `Server::health_stats` (`src/mcp.rs`) JSON carries a `retrieval:` block from
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
  (`src/mcp.rs`) JSON carries `heat_half_life_secs` from `self.cfg.heat`;
  `trnsprt::HealthRes` gains `#[serde(default)] heat_half_life_secs` (old daemon
  → `0`); `kern health` prints `heat: half-life {N}s` daemon-sourced only (item
  100 rule); `kern://local/health` carries it by construction. Proved by dto
  round-trip `2592000` + old-payload absence → `0`, and
  `kern_health_prints_heat_half_life` (30d → `2592000s`, `0` → `0s`, no daemon →
  no line); negative control (omit field → `0` → print reds, green on revert).
  `cargo test -p kern --lib` 955 passed, 0 failed, 4 ignored; `cargo test -p
  trnsprt --lib` 61 passed. Decided by: fix-the-root, name-the-tradeoff,
  verify-before-claiming. Still open: top-10 stability; item 54 GC gate.

- 2026-07-23 — item 83 per-kern entity-count signal: `HealthStats.largest_kern_entities`
  (new field, max `Kern::entities.len()` across resident kerns) — gauge of the
  unbounded resident set at the granularity the kern-cap (bounds count of kerns,
  not size of any one) cannot reach. `kern health` prints `kerns: N (cap M,
  largest L entities)` (or `cap off, largest L entities`), daemon-sourced only.
  MCP `health` JSON carries `largest_kern_entities`; `HealthRes`
  `#[serde(default)]` (old daemon → `0`). Proved by
  `graph_health_stats_reports_largest_kern_entities` (empty → `0`; 10 + four
  empty → `10`), dto round-trip `99`. `cargo test -p kern --lib` 954 passed, 0
  failed, 4 ignored; `cargo test -p trnsprt --lib` 61 passed. Negative control
  (skip the max → `0`) reds, green on revert.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-23 — item 31 `route_entity` clone lever closed: `route_entity`
  (`src/base/accept.rs`) now holds `&kern.children` alongside the `&GraphGnn`
  reborrow in a scoped block (both immutable, borrow ends before
  `current_id = child_id`), dropping the `Vec<String>` alloc per descent —
  bit-identical routing. Proved by `route_entity_does_not_clone_children_per_descent`
  (children vs no-children delta equal within 8 B; re-add `.clone()` of 4 vs 1
  pushes ~72 B past → reds, green on revert). Routing tests green unedited.
  `cargo test -p kern --lib` 953 passed, 0 failed, 4 ignored.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-22 — item 58 trigger #1 instrumented: `supersede` /
  `supersede_by_contradiction` (`src/base/accept.rs`) increment a
  process-global `SUPERSEDE_CHAIN_DEPTH_EXCEEDED` `AtomicU64` when the chain
  depth (via the existing `superseded_ancestors` walk) exceeds
  `SUPERSEDE_CHAIN_HOP_THRESHOLD` (new, `src/base/constants.rs`, default `5` —
  the doc's own number). The counter reads into
  `HealthStats.supersede_chain_depth_exceeded`, folds into `kern health`
  `degraded:` (daemon-sourced only, item 100/28 precedent), and rides MCP
  `health` JSON + `trnsprt::HealthRes` `#[serde(default)]` (old daemon → `0`).
  Proved by `supersede_chain_depth_counter_increments_past_threshold` (6-deep
  → delta 1; 3-deep → 0; serialised on `SUPERSEDE_CHAIN_TEST_MUX` per item 28
  process-global lesson), `graph_health_stats_carries_supersede_chain_depth_exceeded`,
  dto round-trip `: 22`. `cargo test -p kern --lib` 952 passed, 0 failed, 4
  ignored; `cargo test -p trnsprt --lib` 61 passed. Negative control
  (`SUPERSEDE_CHAIN_HOP_THRESHOLD = usize::MAX` → no increment) reds, green on
  revert. Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.
  Still open: rate-limit / `ReasonKind::Edit` decision + triggers #2/#3.

- 2026-07-22 — item 83 signal-on-approach half closed: the armed
  `max_loaded_kerns` (128) is now surfaced. `GraphGnn::max_loaded_kerns()`
  accessor + `HealthStats.max_kerns` + `kern health` prints `kerns: N (cap M)`
  (or `cap off` for `KERN_CAP_DISABLED`) and warns `kerns near cap: N/M` at
  `KERN_CAP_APPROACH_FRAC=0.9` — **daemon-sourced only** (item 100 rule). MCP
  `health` JSON carries `max_kerns`; `trnsprt::HealthRes` gains
  `#[serde(default)] max_kerns` (old daemon → `0` → `cap off`). Proved by
  `graph_health_stats_reports_max_kerns`,
  `kern_health_warns_when_resident_kerns_approach_cap` (116/128 → warn, 10/128
  → none, `u64::MAX`/`0`/no-daemon → none), dto round-trip `max_kerns: 128`.
  `cargo test -p kern --lib` 950 passed, 0 failed, 4 ignored; `cargo test -p
  trnsprt --lib` 61 passed. Negative control (approach check `false` → no warn)
  reds, green on revert. Decided by: fix-the-root, name-the-tradeoff,
  verify-before-claiming.

- 2026-07-22 — item 24 residue #2 closed: `connect_kern` peer-uid check now has
  a test seam mirroring the bind arm's `bind_unix(path, expected_peer)` — a
  `#[cfg(test)]` path injects the expected uid, and
  `connect_kern_refuses_when_the_peer_uid_differs` drives it with
  `geteuid().wrapping_add(1)` against a socket this uid serves, asserting
  `AdapterError::UntrustedEndpoint` naming `served by uid {euid}`. Negative
  control (neuter `require_peer_uid` → foreign uid accepted → test reds, green
  on revert) — same mutation the bind arm test uses. `cargo test -p trnsprt` 61
  passed (+1). The owner-check half was already covered via a root-owned
  `foreign_path()`; this closes the peer-uid half — the gap item 24 named.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-22 — item 52 mechanism half-closed (default-off): `seed_examples`
  (`src/base/accept.rs`) now char-chunks a single long graviton-seed paragraph
  at `GRAVITON_SEED_CHAR_CHUNK` (new, `src/base/constants.rs`, default `4000`)
  — when a single-line seed exceeds the threshold it splits on a code-point
  boundary into `ceil(len/chunk)` chunks returned to the existing caller which
  embeds each + `mean_pool`s (no caller change). Under threshold:
  `vec![text.trim()]` — bit-identical today. Same default-off shape as item 49
  (`DISTILL_CHUNK_TURNS`) and item 57 (`EVIDENCE_HALF_LIFE_SECS=0`). Proved by
  `seed_examples_char_chunks_a_long_single_paragraph` (chunk+5 → 2, each `<=`
  threshold, concat == original) and
  `seed_examples_char_chunks_split_on_a_code_point_boundary` (multibyte `ß` not
  split mid-`char`); `seed_examples_splits_lines_and_keeps_single_text_whole`
  green unedited. `cargo test -p kern --lib` 948 passed, 0 failed, 4 ignored.
  Negative control (force single-chunk path) reds, green on revert.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-22 — item 83 reembed double-alloc half closed: at reembed `vector`
  and `gnn_vector` now share the `Arc` — `e.vector = v.clone().into();
  e.gnn_vector = e.vector.clone();` (`src/tick/tasks.rs`,
  `src/commands/reembed.rs`) — one alloc + one `Arc::clone` instead of two
  `Arc::from(Vec)`, saving ~76.8 MB at 50k/dim384. No COW, no behavior change:
  GNN propagation Arc-swaps `gnn_vector` (never in-place), dropping the shared
  refcount. Proved by `do_reembed_shares_vector_allocation_with_gnn_vector`
  (`Arc::ptr_eq` after `do_reembed`); negative control (revert → not ptr-equal)
  reds, green on revert. `cargo test -p kern --lib` 945 passed, 0 failed, 4
  ignored. Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-22 — item 84 last sub retired: hand-written MCP tool schemas
  accepted as style debt, not a correctness gap. The schemas are hand-written
  JSON in `tools.rs`, correct, unit-tested, and stable; deriving from types
  would need a proc-macro for no correctness/ergonomics/wire gain (the schemas
  ARE the contract an MCP client reads). Item 84 now fully closed. Decided by: the-oracle, name-the-tradeoff. Supersedes: nothing.

- 2026-07-22 — item 84 sub-fix closed: `complete` now retries a transient
  (5xx/429/timeout/connect) with the embed leg's [150,300,600]ms cadence via a
  new `post_with_retry` (`src/llm.rs`) before surfacing the failure — a gateway
  blip no longer re-queues a whole distill transcript. `complete_func` records
  the final failure once; a recovered completion is not counted. The
  Ollama-centric half stays by design (local-first: Ollama-native + OpenAI-compat
  only). New test pins 500-then-ok. 1038 pass. Decided by: fix-the-root, the-oracle.
  Supersedes: nothing.

- 2026-07-22 — item 57 mechanism half-closed (default-off): `decay_evidence`
  (new, `src/tick/stigmergy.rs`) γ-damps `conf_alpha`/`conf_beta` toward the
  Jeffreys prior `(1,1)` by a half-life, gated by `EVIDENCE_HALF_LIFE_SECS`
  (new, `src/base/constants.rs`, default `0` = disabled = bit-identical today).
  For each non-superseded resident entity:
  `conf_alpha = 1.0 + heat::decayed(conf_alpha - 1.0, updated_at, now, half_life)`,
  likewise `conf_beta`, then `refresh_score()`. Decaying `(α-1)`/`(β-1)` toward 0
  keeps `(1,1)` as the floor; `heat::decayed` reused. `run_gc` calls it gated `>
  0` (hourly cadence). `observe_support`/`observe_contradict` now stamp
  `updated_at` so every conf change tracks a timestamp (previously only the
  dedup caller did — a decay using `updated_at` would mis-read GNN-updated conf
  as stale); redundant `accept.rs:153` stamp removed. `updated_at` is existing
  federated state — **no schema/wire change**; broadened "text changed" →
  "mutated". Local-only mutable state, no gossip/wire. Proved by
  `evidence_decay_damps_alpha_beta_toward_prior_by_half_life` (α=11 β=3,
  `now-7d`, half-life 7d → α≈6.0 β≈2.0), `evidence_decay_half_life_zero_is_a_noop`,
  `evidence_decay_skips_superseded_entities`,
  `observe_support_and_observe_contradict_stamp_updated_at`; `dedup.rs:121`
  green unedited. `cargo test -p kern --lib` 942 passed, 0 failed, 4 ignored.
  Negative control: early-return reds, green on revert.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.
  Still open: the policy decision (enable-by-default + rate).

- 2026-07-22 — item 84 sub-fix closed: `kern unnamed promote <id> <name>
  <seed> [--mass N]` promotes an existing unnamed kern to named by giving it a
  graviton in place (no move, no id change — keeps entities/children/parent,
  becomes is_named so gc keeps it). `accept::promote_unnamed` sets
  graviton_text/vec/mass on the existing Kern; CLI embeds seed via
  seed_examples+mean_pool, resolves the short id `kern unnamed` prints, async +
  local (with_graph, guarded flush). 1033 pass, 2 new tests. Decided by: fix-the-root, the-oracle. Supersedes: nothing.

- 2026-07-22 — item 47 (c)/(d) closed: TLS = TOFU pin, network_id =
  config-owned. kern is local-first, zero-config and coordinator-free
  (`VISION.md`); operator PKI needs a CA the operator runs — a coordinator the
  federation refuses to need. TOFU pins the first-seen peer key and warns on
  change (SSH known_hosts shape); trade: a first-contact MITM is undetected,
  mitigated by out-of-band pin verification. Under TOFU there is no cert at
  first contact, so `network_id` stays the operator's `[gossip] network_id`
  (the existing `effective_network_id` guard, not a new one). All 7 item-47
  decisions now recorded; the federation build unblocks on the item 33 transport
  move. No code change. Decided by: name-the-tradeoff, the-oracle, fix-the-root.
  Supersedes: nothing.

- 2026-07-22 — item 47: 5 of 7 federation decisions recorded (the build
  stays blocked on the security pair (c)/(d)). (a) Reason.score LWW — already
  settled by item 13. (b) anti-entropy watermark = content-hash bloom (ids are
  content hashes; a vector clock adds a per-replica counter with no other use; a
  bloom over the live content-hash set is the shape anti-entropy needs anyway).
  (e) graviton mass = per-node, does not federate (mass is local routing tuning;
  federating it lets a peer shift another's acceptance routing silently; the
  graviton's existence federates as content, its mass stays the operator's knob).
  (f) superseded_by conflict = lamport-then-id, not lex-greater-id (lex-greater
  agrees on the wrong successor; lamport-then-id is still deterministic via the
  existing lww_wins tiebreak and picks the later claim). (g) cross-model
  federation = refuse on embed-model mismatch (vectors from two models are
  noise; the store already refuses a mismatched embedder at open, the wire
  extends the same guard — only vector-free CRDT deltas federate across models).
  (c) TLS CA (operator PKI vs TOFU) and (d) network_id source owed — security
  model, user's call; (d) depends on (c). No code change. Decided by: name-the-tradeoff, verify-before-claiming, fix-the-root. Supersedes: nothing.

- 2026-07-22 — item 84 pure-rename half closed (rename re-keying complete):
  `supersede_renamed` (`src/base/accept.rs:578`) gains `new_external_id: &str`
  and on `old_id == new_id` (content-unchanged rename) re-keys the survivor —
  `entity.external_id = new_external_id`, `clear_source_entry(old)`,
  `set_source_entry(new)`, then `return None` (no supersede edge — same entity).
  The `file_watcher.rs` caller passes `source.object_id()` (the new path). The
  rename+edit half (`old_id != new_id`) closed earlier in `789968a`; together a
  renamed file stops leaving the `Document` under its stale old path whether or
  not it was edited. Proved by `a_pure_rename_re_keys_the_survivor_external_id`
  (survivor `external_id` == new path, old source-index cleared, new set,
  survivor active); negative control (revert to bare `return None` →
  `external_id` stays old path) reds, green on revert.
  `a_rename_plus_edit_supersedes_the_old_path_document` and
  `a_rename_with_no_old_entity_is_a_noop` green unedited. `cargo test -p kern
  --lib` 937 passed, 0 failed, 4 ignored.
  Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.

- 2026-07-22 — item 60 re-classification wiring closed: when an entity
  carrying a deferred Rephrase candidate is superseded by a different update,
  stamp_superseded now re-points the candidate's from to the new active entity
  and pushes (kern_id, reason_id) onto a new GraphGnn::pending_reclass set; the
  tick loop drains it and re-enqueues ClassifyContradiction, so the candidate
  re-classifies against the new claim instead of orphaning on
  do_classify_contradiction's old.is_superseded() early return. The queue is an
  acceleration (the re-pointed Rephrase persists, so a restart classifies
  anyway). New test pins re-point + queue. 1030 pass. Item 60 fully closed
  (belief half + reclass wiring). Decided by: fix-the-root, name-the-tradeoff,
  verify-before-claiming. Supersedes: nothing.

- 2026-07-22 — item 60 belief half closed by decision: Reason edges carry
  belief directionally (not symmetrically — Provenance/Question/Supersedes are
  directional by construction; symmetry would conflate a vouch with its
  reverse), and superseding resets belief (a supersede is a new claim that mints
  its own beta prior; inheriting the old's evidence would read a single
  observation as well-evidenced). Both match shipped behaviour — no code change.
  The re-classification wiring (re-point a deferred Rephrase to the replacer
  when one side of a pair is superseded) stays open. Decided by: name-the-tradeoff,
  verify-before-claiming. Supersedes: nothing.

- 2026-07-22 — item 49 chunking half-closed: `distill` (`src/ingest/distill.rs`)
  now batches a long conversation into turn-groups of `DISTILL_CHUNK_TURNS`
  (new, `src/base/constants.rs`, default `48`), calls `llm` + `parse_claims`
  per batch, and concats the claims — so a long delta stops truncating past the
  model's context window with no signal. The common case (`turns.len() <=
  batch`) stays one call, bit-identical to today. Turn-batched (not char-batched)
  preserves the 1-based turn markers `split_turns` produces, so
  `Source::Session.section` turn-citations stay well-formed per chunk. A batch
  returning no parseable array (prose / empty) is a format failure for the
  **whole delta** → `None` (retry), so a partially-distilled conversation never
  archives having silently dropped every later batch. Proved by
  `distill_short_conversation_is_one_call`,
  `distill_chunks_long_conversation_turn_batched`,
  `distill_chunk_markers_carry_global_turn_index`,
  `distill_batch_format_failure_retries_whole_delta`. Existing distill tests
  green unedited. `cargo test -p kern --lib` 932 passed, 0 failed, 4 ignored.
  Negative control: `DISTILL_CHUNK_TURNS=usize::MAX` reds the chunk test, green
  on revert. Decided by: fix-the-root, name-the-tradeoff, verify-before-claiming.
  Still open: per-kind branch / label-accuracy half (the ~33% figure is
  unreproducible, a lead not a number).

- 2026-07-22 — item 75 cross-segment-atomicity half closed: `build_and_save`
  now builds into a staging dir, `atomic_write` fsyncs each segment, the staging
  dir is fsync'd, and the publish is ONE rename of the staging dir over the live
  dir. Three independent renames used to leave meta from build N+1 beside vectors
  from build N if a crash hit between them — and `open`'s shape checks pass
  whenever the two builds share count/dim/r, the common case. Now a crash before
  the swap leaves the old build intact; a crash in the remove→rename window
  leaves no index, which is non-fatal (`build_entity_disk_snapshot` falls back to
  the in-RAM index) — silent staleness until next rebuild, never a mixed-build
  read. New test pins two consecutive builds over one dir (second is whole, no
  staging lingers). POSIX-only: Windows cannot delete an open file so a
  concurrent reader would fail the swap; DiskANN is off-by-default, Linux-first.
  1025 pass. Decided by: fix-the-root, verify-before-claiming, name-the-tradeoff.
  Supersedes: nothing.

- 2026-07-22 — item 83 resident-cap half closed: `GraphConfig::default().
  max_kerns` is now 128 (was `KERN_CAP_DISABLED`/`usize::MAX`). The old
  "currently unsafe — eviction drops unpersisted children pushes" comment was
  stale, verified: `get_mut` auto-loads from the store, so a parent evicted
  inside `spawn_unnamed_child`'s `register` is reloaded by the post-register
  `get_mut` and the children-push persists — no re-spawn loop. A new test pins
  it under `max_kerns = 2` with a store bound. 128 is a conservative resident
  bound (normal use <10 kerns); eviction unloads to the cold tier, never
  forgets; `usize::MAX` still opts out. `disk_threshold` stays disabled until
  item 75 (DiskANN crash consistency) closes — arming it exposes the spill
  crash window. 1024 pass. Decided by: verify-before-claiming, fix-the-root, name-the-tradeoff. Supersedes: the stale "currently unsafe" comment in `GraphConfig::default`.

