# kern — agent notes

Read `docs/ORACLE.md` before acting.

## Format compatibility

kern 2.0 is released, not alpha: a store written by the previous release must
keep opening. When a persisted or wire format changes, bump the single live
format version (`FORMAT_VERSION` in `src/store_core/src/lib.rs`,
`WEIGHT_FILE_VERSION` in `src/gnn.rs`) and carry three things in the same
commit — this replaced "old stores are wiped and reingested" on 2026-08-16,
once a store held a real corpus and wiping it stopped being affordable:

1. the outgoing layout, frozen in `src/store_core/src/legacy.rs` (decode-only —
   nothing may ever write a legacy type) and listed in `READABLE_VERSIONS`;
2. an arm in `decode_kern_row` / `decode_cold` for the old version byte;
3. an updated checksum in `src/store_core/src/tests/layout_guard.rs`, which
   fails the build when a persisted struct changes so a bump cannot be
   forgotten. It was forgotten once (f60fbce added `Entity.trust_tier` with no
   bump), which is why two mutually incompatible layouts both call themselves
   version 10.

Nothing is *sniffed* into a different version, and an unreadable version is
refused by name rather than guessed at. Reading an old store converts it in
memory only; `kern migrate` is what rewrites it on disk. `READABLE_VERSIONS`
holds one hop back — a store more than one release behind is rejected at
load, not silently wiped: `kern doctor` names it, and the fix is to migrate
forward through the intermediate release first, or wipe and reingest by hand.

Wire-format (the daemon RPC) still gets no compatibility promise across
non-patch releases: DTOs decode tolerantly (`#[serde(default)]` throughout
`src/transport/src/kern_rpc.rs`) so a CLI upgraded slightly ahead of a
long-running detached daemon does not hard-fail, but that is a grace window,
not a contract — restart the daemon on a real upgrade.

## Memory (kern)

- At task start: call kern `query` with the task topic to recall prior
  decisions, preferences, and facts before deciding anything.
- At task end, and whenever a durable decision, preference, constraint, or
  hard-won fact emerges: call kern `ingest` with ONE self-contained statement
  per fact. Include the why on decisions.
- When recall returns something wrong or stale: call `degrade` with the query
  id so it stops surfacing.
