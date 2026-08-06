# Ideas

Living list of what to combine, simplify and smooth in this project, each
item carrying the grep that produced it. Driven by the `/improve` skill.

## A. Combine
## B. Simplify

### B6. Dead `pub fn is_semantic` — unused ReasonKind predicate

**Claim:** `pub fn is_semantic(self) -> bool` on `ReasonKind` in
`src/base/types.rs` classifies `Similarity | Provenance | Ratification`. The
enum variants are live (Similarity used in tick/accept/commands/query), but
the *predicate* has zero callers — nothing asks "is this semantic?".

**Evidence:**
```
src/base/types.rs:124:	pub fn is_semantic(self) -> bool {
src/base/types.rs:125:		matches!(self, ReasonKind::Similarity | ReasonKind::Provenance | ReasonKind::Ratification)
```
`rg -n '\bis_semantic\b' src/ -g '*.rs' | grep -v 'fn is_semantic' | grep -v '//.*is_semantic'`
→ no hits.

**Do:** delete the fn (6 lines incl. match block).

**Payoff:** removes 1 dead `pub` predicate. Pure deletion.
**Size:** tiny, one sitting.

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

### B3. Test-only `add_graviton` kept alive by a `dead_code` escape hatch

**Claim:** `pub(crate) fn add_graviton(g, name, vec)` in `base/accept.rs` is a
thin wrapper `add_graviton_with_mass(g, name, vec, 1.0)` marked
`#[cfg_attr(not(test), allow(dead_code))]` — it is never called outside
`#[cfg(test)]`. Two ways to add a graviton, one of them dead in prod and
propped up by an escape hatch the persona says to delete first.

**Evidence:**
```
src/base/accept.rs:846:#[cfg_attr(not(test), allow(dead_code))]
src/base/accept.rs:847:pub(crate) fn add_graviton(g: &mut GraphGnn, name: &str, vec: Vec<f32>) {
src/base/accept.rs:848:	add_graviton_with_mass(g, name, vec, 1.0)
}
```
Callers (all under `#[cfg(test)]`): 8 in `src/base/accept.rs::tests`, 2 in
`src/retrieval/gravity.rs::tests` (via `use crate::base::accept::add_graviton`).
`rg -n 'add_graviton\b' src/` → no non-test hits.

**Do:** delete the fn + the `#[cfg_attr(not(test), allow(dead_code))]` line;
inline `add_graviton_with_mass(g, name, vec, 1.0)` at all 10 test call sites;
drop the `use …add_graviton` import in gravity.rs, add
`use …add_graviton_with_mass`.

**Payoff:** removes 1 `pub(crate)` fn + 1 dead_code escape hatch + 1 cross-
module test import; collapses two graviton-add APIs to one. Net -3 lines,
+0 chars-per-call-site. Pure deletion of a second way.
**Size:** small, one sitting. Test-only, no public API change.

## C. Smooth
## D. Fix

### B4. Hand-rolled `parse_ipv4` where std `Ipv4Addr::from_str` does the same

**Claim:** private `fn parse_ipv4(host) -> Option<[u8;4]>` in `src/llm.rs` hand-
rolls a 4-octet parser; `std::net::Ipv4Addr::from_str` parses the same 4-
octet form and `.octets()` yields `[u8;4]`. Two callers (`is_local_url`,
`is_loopback_url`) only feed the octets to `is_local_ipv4` / a `o[0]==127`
check. Persona #3 (std over hand-rolled) + #1 (delete).

**Evidence:**
```
src/llm.rs:642:fn parse_ipv4(host: &str) -> Option<[u8; 4]> {
src/llm.rs:588:  if let Some(o) = parse_ipv4(host) { return is_local_ipv4(&o); }
src/llm.rs:612:  if let Some(o) = parse_ipv4(host) { return o[0] == 127; }
```
`rg -n 'parse_ipv4' src/` → no other callers.

**Do:** delete `parse_ipv4`; inline
`if let Ok(a) = host.parse::<Ipv4Addr>() { … a.octets() … }` at both sites;
add `use std::net::Ipv4Addr;`.

**Payoff:** removes 1 hand-rolled parser (8 lines), uses std. Net -6 lines.
**Narrowing:** std rejects leading-zero/ambiguous octet forms the manual
parser accepted; only relevant for local-egress detection on config URLs where
such forms do not occur. Existing tests use clean IPs (127.0.0.1, 10.0.0.1,
172.16.0.1, 192.168.1.1, 169.254.0.1, 203.0.113.5) — all parsed identically.
**Size:** small, one sitting. Private fn, no public API change.

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

## B. Simplify

### B1. Duplicated hex-decode loops; no canonical `hex::decode`

**Claim:** five manual hex-decode loops re-implement what should be one
`hex::decode` in `base::util::hex` (which today has only `encode`). Two of
them (`decode_hex_32` in identity.rs, `parse_key_hex` in contract.rs) are
near-identical 32-byte decoders; the latter is pub and the former is a
private duplicate.

**Evidence:**
```
src/gossip/identity.rs:163:    *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
src/gossip/contract.rs:121:    *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
src/gossip/privacy.rs:58:    bytes.push(u8::from_str_radix(&hex[i..i + 2], 16).ok()?);
src/mcp/tools_delegate.rs:169: .map(|i| u8::from_str_radix(&sig_hex[i..i + 2], 16).unwrap())
src/mcp/tools_delegate.rs:241: .map(|i| u8::from_str_radix(&sig_hex[i..i + 2], 16).unwrap())
```
`base::util::hex` surface today: `encode` only (src/base/util.rs:13).

**Do:**
1. Add `pub fn decode(s: &str) -> Option<Vec<u8>>` to `base::util::hex`
   (hexdigit + even-length check, one loop).
2. `parse_key_hex` (contract.rs) -> strip prefix, `hex::decode`, len==32
   check, `try_into`.
3. Delete `decode_hex_32` (identity.rs) -> call `parse_key_hex` (or
   `hex::decode` + `try_into`).
4. `decrypt_text` (privacy.rs) -> `hex::decode` after prefix strip.
5. Two test loops in mcp/tools_delegate.rs -> `hex::decode` (sigs are 64
   bytes; `parse_key_hex` is 32-only, so the canonical `hex::decode` is the
   right call here).
Add one test for `hex::decode` (round-trip with `encode`, odd-length ->
None, non-hex -> None).

**Payoff:** removes 4 manual loops + 1 private fn; one authoritative
decoder. ~25 lines out, ~12 in (helper + test). Net deletion.
**Size:** small, one sitting. Crosses `base`, `gossip`, `mcp` — gate fires
(see step 0); add of a `pub fn` is an internal-crate API addition.
