# Ideas

Living list of what to combine, simplify and smooth in this project, each
item carrying the grep that produced it. Driven by the `/improve` skill.

## A. Combine
## B. Simplify
## C. Smooth
## D. Fix

## Highest payoff first

_(ranked across all four sections)_

1. **B2 (inherited, red)** — finish the `is_error` centralization WIP
   stashed this fire (`git stash list`): `is_error` in `src/mcp/tools.rs`
   is `pub(crate)` but used only in `#[cfg(test)]` modules, so the non-test
   lib build flags it `dead_code` under `-D warnings`. Mark it `#[cfg(test)]`
   (or move under a test-support module) and remove the now-redundant local
   `is_error` copies already deleted by the WIP in tools_query/admin/events/
   mutate/delegate. Payoff: unblocks `just check`; size small. Evidence:
   `rg -n "pub\(crate\) fn is_error" src/mcp/tools.rs` + the stashed diff.
2. **D1 (noise, not this fire)** — pre-existing clippy red on `main`:
   `just check` (clippy `-D warnings`) already fails on HEAD in files untouched
   by B1: `len_zero`, `div_ceil`, `unnecessary-get-then-check`, `too_many_arguments`,
   `is_multiple_of`, `assert_eq!` literal bool. ~12 errors across
   base/accept.rs, gossip/node.rs, gossip/transport.rs, ingest/distill.rs,
   ingest/file_watcher.rs, tick/stigmergy.rs. Confirm: `git stash` (clean tree)
   then `cargo clippy --all-targets` → red. Out of scope for B1; pick per item.

## Closed

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
