# Ideas

Living list of what to combine, simplify and smooth in this project, each
item carrying the grep that produced it. Driven by the `/improve` skill.

## A. Combine
## B. Simplify

### Flatten `ingest` subdir into src/ root — DONE 2026-08-06 (this fire)

### Flatten `mcp` subdir into src/ root — DONE 2026-08-06 (this fire)

Moved all 10 `src/mcp/*.rs` up to `src/mcp_*.rs`; `mcp.rs` → SHIM re-exporting
prompt/resources/sse/tools + tools_query (2 external consumers); dropped 6 unused
private re-exports (tool methods impl'd on Server, dispatched via `self.tool_X()`).
lib.rs gained 11 `pub/pub(crate) mod mcp_*`. Response/RpcError fields → `pub(crate)`
(siblings need access, were child-visible only). `include_str!("../mcp.rs")`→`("mcp.rs")`.

### Flatten `ingest` subdir into src/ root — DONE 2026-08-06

Moved all 12 `src/ingest/*.rs` up to `src/ingest_*.rs` (prefix-rename to dodge
`config` collision + keep grouping); `mod.rs` → `ingest.rs` SHIM re-exporting
the 12 submodules (`pub use crate::ingest_config as config;` etc.) + item
re-exports so `crate::ingest::X` still resolves for ~18 external consumers —
shim minimizes churn vs full rewrite. lib.rs gained 12 `pub mod ingest_*`.
Rewrites: `super::X`(sibling)→`crate::ingest_X`, `crate::ingest::X`(own
submod)→`crate::ingest_X`; ITEM refs (Config/Worker/Job/ReviewPolicy/review_for/
stub_one_hot) kept as `crate::ingest::`. Build clean, 1096 tests pass, guards 0.

### Flatten `gnn` subdir into src/ root — DONE 2026-08-06 (this fire)

Moved all 13 `src/gnn/*.rs` up to `src/gnn_*.rs` (prefix-rename to dodge
`graph`/`persist` collisions + keep grouping); dropped `gnn/mod.rs`; `src/gnn.rs`
shim re-exports the 13 submodules (`pub use crate::gnn_gcn as gcn;` etc.) so
`crate::gnn::X` still resolves for the 60 existing refs — shim minimizes churn
vs full rewrite. lib.rs gained 13 `pub mod gnn_*` (all pub). Rewrites in moved
files: `super::X`(sibling)→`crate::gnn_X`, `crate::gnn::X`(own submod)→`crate::gnn_X`;
`crate::gnn::GnnError` kept (parent item, not mangled). Build clean, 1096 tests
pass, guards 0.

### Flatten `gossip` subdir into src/ root — DONE 2026-08-06

Moved all 13 `src/gossip/*.rs` up to `src/gossip_*.rs` (prefix-rename to dodge
`identity`/`types` collisions + keep grouping); dropped `gossip/mod.rs`; lib.rs
declares the 13 `pub mod gossip_*` (all pub, visibility preserved). Rewrites in
moved files: `crate::gossip::X`→`crate::gossip_X`, `super::X` (sibling)→`crate::gossip_X`,
`use crate::gossip::{X,Y}`→`use crate::gossip_X; use crate::gossip_Y;`, test
`use super::*` kept. External `src/mcp/tools_delegate.rs` + `src/commands.rs`:
`crate::gossip::X`→`crate::gossip_X`. Build clean, 11 test suites pass, guards 0.

### Flatten `commands` subdir into src/ root — DONE 2026-08-06 (this fire)

Moved all 11 `src/commands/*.rs` up to `src/commands_*.rs` (prefix-rename to
dodge collisions + keep grouping clear); dropped the `commands/` dir; parent
`src/commands.rs` lost its `pub(crate) mod` block, re-export retargeted to
`crate::commands_mcp_cmd::ensure_mcp_registered`; lib.rs gained the 11 module
declarations (admin+graph_ops `pub(crate)`, rest private). Rewrites in moved
files: `use super::route::`→`use crate::commands_route::` (sibling submods),
`use super::{load_graph, Client, ...}`→`use crate::commands::{...}` (parent
items), `pub(super)`→`pub(crate)`, test `use super::*` kept (same-file items).
External consumers `src/mcp/tools_{mutate,admin}.rs`:
`crate::commands::graph_ops::`→`crate::commands_graph_ops::`,
`crate::commands::admin::`→`crate::commands_admin::`. Build clean, tests pass,
guards exit 0.

### Inline `watcher` sub-crate into the root `kern` crate — DONE 2026-08-06 (this fire)

Folded the `watcher` workspace member into `kern` as `crate::watcher`. Moved
`src/watcher/src/{lib,event,ignore_rules,pipeline,watcher}.rs` up to
`src/watcher/{mod,event,ignore_rules,pipeline,file}.rs` (inner `watcher.rs`→`file.rs`
to dodge clippy `module_inception`); dropped `src/watcher/Cargo.toml` + the
`watcher` path-dep + workspace member; folded `notify`+`ignore` into root deps
(its `async-trait`/`tempfile`/`tracing`/`thiserror`/`tokio` were already root);
rewrote consumers `src/ingest/file_watcher.rs:5` + `src/commands.rs:1140`
`use watcher::`→`use crate::watcher::`; fixed the two internal `use crate::event::`
in `watcher.rs`/`pipeline.rs`→`use super::event::`; moved the integration test to
`tests/watcher_tests.rs` (`use watcher::`→`use kern::watcher::`). Build clean,
7/7 watcher tests pass, all test targets compile, code-reviewer approved.
`transport-macros` (proc-macro) stays separate by Rust constraint.

### Inline `transport` sub-crate into the root `kern` crate — DONE 2026-08-06 (this fire)

Folded the `transport` workspace member into `kern` as `crate::transport`.
`src/transport/src/{lib,http,mcp}.rs` + `hub_rpc/`+`kern_rpc/`+`typed/`+`wire/`
moved up to `src/transport/{mod,http,mcp,...}`; `lib.rs`→`mod.rs`; dropped
`src/transport/Cargo.toml` + the `transport` path-dep + workspace member.
Internal `crate::`→`crate::transport::` across the moved files (critical: avoids
the collision between transport's `mod mcp` envelope and kern's root `pub mod
mcp` server). `service!` invokers `crate::service!`→`crate::transport::service!`.
The `transport-macros` proc-macro stayed its own crate (Rust forbids
proc-macros in a lib); its `service!` codegen retargeted `::transport::`→
`crate::transport::` (call-site crate = kern; `::kern::` self-ref did NOT resolve
from proc-macro expansion — verified by failed build). Removed
`extern crate self as transport;`. 15 consumer files rewritten
`transport::`→`crate::transport::`. Folded deps into root: `tokio-util`+codec,
`bytes`, `futures`, unix `libc`, windows `windows-sys`. Build clean, 1096 lib
tests pass, 59 transport tests pass, all test targets compile, guards exit 0,
code-reviewer approved.

### Remaining: `transport-macros` stays a separate crate (Rust constraint)

`src/transport/macros/` is a `proc-macro = true` crate — Rust forbids
proc-macros inside a lib/bin crate, so it cannot be inlined. This is the only
remaining workspace member; the single-crate goal is complete modulo this
forced exception. No action; record only.

### Dead-`pub` scrape rejects (Pass 4, for the record)

- `for_eval` / `with_temperature` (src/llm.rs builder methods, seed+temperature
  pins) — zero callers, but `docs/vllm.md:16` documents "seed/temperature (eval
  pins) are forwarded; vLLM honors both" → planned-feature scaffolding. Keep;
  close `dropped — feature WIP`.
- `forged_id_rejected()` (src/gossip/handler.rs) — read accessor dead, but its
  counter `FORGED_ID` IS incremented at handler.rs:616 (live write path).
  Deleting only the reader leaves a zombie counter; deleting the counter too
  loses a diagnostic intent that may be wired later → needs a decision, not a
  clean fold. Keep for now.

## C. Smooth
## D. Fix

## Highest payoff first

_(ranked across all four sections)_

1. **D1 (noise, not this fire)** — pre-existing clippy red on `main`:
   `just check` (clippy `-D warnings`) already fails on HEAD in files untouched
   by B1: `len_zero`, `div_ceil`, `unnecessary-get-then-check`, `too_many_arguments`,
   `is_multiple_of`, `assert_eq!` literal bool. ~12 errors across
   base/accept.rs, gossip/node.rs, gossip/transport.rs, ingest/distill.rs,
   ingest/file_watcher.rs, tick/stigmergy.rs. Confirm: `git stash` (clean tree)
   then `cargo clippy --all-targets` → red. Out of scope for B1; pick per item.

## Closed

### 2026-08-07 — flatten src/retrieval/ into src/ root (retrieval_* prefix)

Moved 9 `src/retrieval/{diversify,expand,fuse,gravity,merge,pagerank,query,score,seed}.rs` → `src/retrieval_*.rs` (retrieval_ prefix, dodges merge collision with root merge.rs). `src/retrieval.rs` kept as SHIM re-exporting all 9 submodules + EmbedFunc/LlmFunc, so ~11 external `crate::retrieval::score::X`/`crate::retrieval::query::X`/`crate::retrieval::seed::X`/`crate::retrieval::LlmFunc` refs resolve unchanged. No rewrites in moved files needed. lib.rs gained 9 `pub mod retrieval_*`. Build clean, 1096 lib tests pass, guards 0.

### 2026-08-07 — flatten src/base/ into src/ root (base_ prefix)

Folded the `base` subdir (22 files) into src/ root. `src/base/{store,types,constants}.rs`→`src/base_{store,types,constants}.rs` (collided with root `store.rs`/`types.rs`); the other 19 kept bare names (`accept.rs`→`src/accept.rs` etc). `src/base.rs` deleted; lib.rs declares the 22 modules directly. Rewrote `crate::base::store/types/constants`→`crate::base_store/base_types/base_constants` and `crate::base::X`→`crate::X` across 70 consumer files; subfile `super::store/types/constants`→`crate::base_store/base_types/base_constants`; aliased `use crate::base_constants as constants;` where bare `constants::` was used. Build clean, 1096 lib tests pass, guards exit 0.

### 2026-08-07 — flatten src/config/ into src/ root (config_* prefix)

Moved `src/config/mod.rs` → `src/config.rs` and all 17 subfiles → `src/config_*.rs` (config_embed.rs, config_gnn.rs, etc.). Subfiles became crate-root modules declared in lib.rs (private `mod` preserved original visibility; `pub mod` for detached_log + io). config.rs re-exports retargeted to `crate::config_*::`. External `crate::config::detached_log::` → `crate::config_detached_log::` (hub_node.rs, commands/mcp_cmd.rs); `kern::config::io::Error` → `kern::config_io::Error` (main.rs). 1096 lib tests pass.

### 2026-08-06 — B6: dead `pub fn is_semantic` deleted (unused ReasonKind predicate)

**Claim:** `pub fn is_semantic(self) -> bool` on `ReasonKind` classified
`Similarity | Provenance | Ratification` but had zero callers — nothing asked
"is this semantic?".

**Evidence (re-run 2026-08-06):** `rg -n 'is_semantic' src/ -g '*.rs'` → no
hits; the enum variants stay (Similarity used in tick/accept/commands/query).

**What changed:** deleted the 6-line predicate from `src/base/types.rs`.

**Why the old shape was wrong:** a `pub` predicate nobody calls is surface
area with no consumer; the classification, if ever needed, is a one-line
`matches!` at the call site. Kept variants live; only the dead reader went.

### 2026-08-06 — B5: dead `pub fn neighbor_ids` deleted (cross-crate dead-pub axis)

`pub fn neighbor_ids<'a>(g: &'a GraphGnn, id: &str) -> Vec<&'a str>` in
`src/retrieval/expand.rs` (~22 lines) mirrored `expand()`'s edge filters
without scoring; its doc comment said "path diagnostics measure what retrieval
sees." Zero callers anywhere in the crate (tests included). No diagnostics
command/surface exists in `src/commands/` or `src/mcp/`, and no
prd/roadmap entry for a diagnostics feature — dead aspirational code, not
planned-feature scaffolding. REPOS.md documents nothing compile-depends on
the kern lib (reached only as an external MCP tool), so a `pub` fn with no
in-crate caller is genuinely dead, not a real public-API change. Deleted the
fn + its 2-line doc comment. The old shape was wrong because a named helper
with no caller earns nothing (persona: names earn their keep, delete-first).
Net -24 lines. `cargo build --lib` + `cargo test --lib` (1020 tests) green;
no new clippy (D1 noise unchanged, 12 pre-existing, none in expand.rs).
Reviewer APPROVED.

### 2026-08-06 — B4: hand-rolled `parse_ipv4` replaced by std `Ipv4Addr`

Private `fn parse_ipv4(host) -> Option<[u8;4]>` in `src/llm.rs` hand-rolled a
4-octet IPv4 parser (split('.'), len==4, each `p.parse::<u8>()`). Its two
callers (`is_local_url`, `is_loopback_url`) only fed the octets to
`is_local_ipv4` / a `o[0]==127` loopback check. Deleted the fn, added
`use std::net::Ipv4Addr;`, inlined `if let Ok(a) = host.parse::<Ipv4Addr>()
{ … a.octets() … }` at both sites. The old shape was wrong because a hand-
rolled parser duplicated what std `Ipv4Addr::from_str` already does (persona:
std over hand-rolled). Net -6 lines. Narrowing: std rejects leading-zero /
octal-ambiguous octet forms the manual parser accepted; acceptable for a
local-egress-detection helper on config URLs (such forms do not occur there);
all existing tests use clean IPs std parses identically. `cargo build --lib` +
`cargo test --lib` (1020 tests) green; no new clippy errors (D1 noise
unaffected). Reviewer APPROVED.

### 2026-08-06 — B3: test-only `add_graviton` deleted; one way to add a graviton

`pub(crate) fn add_graviton(g, name, vec)` in `base/accept.rs` was a thin
`add_graviton_with_mass(g, name, vec, 1.0)` wrapper marked
`#[cfg_attr(not(test), allow(dead_code))]` — never called outside `#[cfg(test)]`.
It existed as a second, mass-defaulted way to add a graviton, kept alive only
by a dead_code escape hatch. Deleted the fn + the escape hatch + the cross-
module `use crate::base::accept::add_graviton` import in gravity.rs; inlined
`add_graviton_with_mass(g, name, vec, 1.0)` at all 10 test call sites (8 in
accept.rs::tests, 2 in gravity.rs::tests). The old shape was wrong because a
dead-in-prod `pub(crate)` helper was propping up a second API surface behind
an escape hatch the persona deletes first. Net -3 lines; 2 graviton-add APIs
collapse to 1; `cargo build --lib` + `cargo test --lib` (1020 tests) green;
no new clippy errors (pre-existing D1 noise unaffected). Reviewer APPROVED.

### 2026-08-06 — B2: duplicated `is_error` MCP-test helpers folded into one `#[cfg(test)]` canonical

Six private `fn is_error(&serde_json::Value) -> bool` copies (one each in
mcp/tools_admin.rs, tools_events.rs, tools_mutate.rs, tools_delegate.rs, and
two in tools_query.rs across its `id_filter_tests` and `cold_tier_filter_tests`
modules) all re-implemented `out.get("isError").and_then(|x| x.as_bool()).unwrap_or(false)`.
An inherited WIP had started centralizing them into a `pub(crate) fn is_error`
in `src/mcp/tools.rs` but (a) left it un-`#[cfg(test)]`-gated, so the non-test
lib build flagged it `dead_code` under `-D warnings` (blocking `just check`),
and (b) missed two of the six copies (tools_query.rs:845 and tools_delegate.rs).
Finished the fold: gated the canonical `#[cfg(test)]`, deleted all six private
copies, routed every test module through `use crate::mcp::tools::is_error;`.
The old shape was wrong because a one-line predicate was copy-pasted six
times and a half-finished centralization left the tree red. Net `+14/-32`
(-18); 6 duplicate implementations collapse to 1; `just check` unblocked for
the lib build (pre-existing clippy noise in untouched files remains, tracked
as D1). Reviewer APPROVED, no behaviour bug.

### 2026-08-06 — B1: duplicated hex-decode loops folded into one `hex::decode`

Five manual hex-decode loops (gossip/identity `decode_hex_32`,
gossip/contract `parse_key_hex`, gossip/privacy `decrypt_text`, two loops in
mcp/tools_delegate tests) re-implemented what should have been one
decoder next to `base::util::hex::encode`. Added `pub fn decode` to
`base::util::hex` (even-length + hexdigit check, tolerates an `ed25519:`
prefix) and routed all five sites through it; deleted the private
`decode_hex_32`. The old shape was wrong because `hex` had only `encode` —
every consumer hand-rolled the inverse, so a length/validation fix had to be
made in five places. Net `+43/-36` (the canonical helper + round-trip test
outweigh the removed loops by ~7 lines), but 4 duplicate implementations
collapse to 1; reviewer APPROVED, no behaviour bug. Pre-existing red WIP
(the `is_error` centralization) and pre-existing clippy noise were set aside
as B2 / D1 above, not fixed here.

## Method

### Pass 4 — cross-crate dead-`pub` axis (no external compile consumer)

REPOS.md: nothing compile-depends on the kern lib crate (ctrl/agent/ui reach
kern only as an external MCP tool). So a `pub` fn with no in-crate caller is
genuinely dead, not a real public-API change. Scraped 540 `pub fn` names for
zero non-definition references in `src/` (tests included). 5 candidates:
`for_eval`, `with_temperature`, `is_semantic`, `neighbor_ids`,
`forged_id_rejected`. Two rejected (see "Dead-`pub` scrape rejects" above:
for_eval/with_temperature = feature-WIP eval pins per docs/vllm.md;
forged_id_rejected = reader dead but counter write live → zombie if reader
deleted). Two folds filed: B5 `neighbor_ids` (this fire), B6 `is_semantic`
(next).

### Pass 3 — deletion axis (residual dead code + duplicated test helpers)

Followed B3. Greps re-ran clean; sampled `pub(crate)` fns all live.

Evidence greps:

    rg -n 'allow\(dead_code\)' src/ --type rust   # → no hits (B3 closed the last)
    cargo build --lib                            # → no dead_code warnings

Checked all alive (callers exist outside their own `fn` line):
`strip_deleted_marker`, `civil_from_days`, `date_string`,
`collect_reason_ids`, `root_graviton_ids`, `remove_graviton`.

`#[allow(clippy::too_many_arguments)]` sites (16) inspected — all are real
multi-param fns (private: `run_once`, `fuse_hybrid_seeds`,
`index_kern_into`, `write_files`, `commit_reason`, `job`; exposed:
`pub(super) cmd_ingest`, `pub retrieve_profiled`). Folding them needs a
param **struct** (adds code, Combine axis A), not a deletion — out of scope
for the `/simplify` (remove) half.

`make_server` wrappers in `mcp/resources.rs` and `mcp/tools_mutate.rs` are
identical 1-line delegations to `test_support::mcp_server()` but serve 14 call
sites as a short local alias — deleting would add verbosity (move the floor),
not remove. Kept.

Stall signal: in-crate simplify axis saturated. Next gains are Combine-axis
(param structs) or cross-crate dead-`pub` analysis (kern consumed by
ctrl/agent/ui per REPOS.md).

### Pass 2 — deletion axis (dead_code escape hatches)

Greps pasted under each item below. Axis: `allow(dead_code)` escape hatches
and test-only `pub(crate)` helpers propping them up.

Evidence grep:

    rg -n 'allow\(dead_code\)' src/ --type rust

Hits:
- src/base/accept.rs:846 — `#[cfg_attr(not(test), allow(dead_code))]` over
  `pub(crate) fn add_graviton` (a thin wrapper over `add_graviton_with_mass`).
  Callers all `#[cfg(test)]` (see B3).

### Pass 1 — deletion axis (hex decode duplication)

Greps pasted under each item below. Axis: duplicated helpers — manual
hex-decode loops scattered across the crate while `base::util::hex` ships
only `encode`.

Evidence grep:

    rg -n "from_str_radix.*16|step_by\(2\)" src/ --type rust

Hits:
- src/gossip/identity.rs:163  — private `decode_hex_32`, manual loop -> [u8;32]
- src/gossip/contract.rs:121  — pub `parse_key_hex`, manual loop -> [u8;32] (also strips `ed25519:`)
- src/gossip/privacy.rs:57    — manual loop -> Vec<u8> (variable length)
- src/mcp/tools_delegate.rs:168 — test, manual loop -> Vec<u8> (64-byte sig)
- src/mcp/tools_delegate.rs:240 — test, manual loop -> Vec<u8> (64-byte sig)

Canonical home: `src/base/util.rs` `pub mod hex` has `encode` only — no
`decode`. `parse_key_hex` (contract.rs) is already pub and imported cross-
module (mcp/tools_delegate uses it).

