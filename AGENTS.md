# kern — agent notes

Read `docs/windmill/ORACLE.md` before acting.

## Alpha — no compatibility

kern is version alpha. Features we change need **no** backward compatibility:
no serde aliases for renamed fields, no wire-format stability across builds.
When a persisted or wire format changes, bump the single live format version
(`FORMAT_VERSION` in `src/store_core/src/lib.rs`, `WEIGHT_FILE_VERSION` in
`src/gnn.rs`).

**Amended 2026-08-16 (user-directed): a store is migrated one hop, not wiped.**
"Old stores are wiped and reingested" was affordable while stores were empty;
it stopped being affordable the moment one held a corpus. A bump now carries
three things in the same commit:

1. the outgoing layout, frozen in `src/store_core/src/legacy.rs` (decode-only —
   nothing may ever write a legacy type) and listed in `READABLE_VERSIONS`;
2. an arm in `decode_kern_row` / `decode_cold` for the old version byte;
3. an updated checksum in `src/store_core/src/tests/layout_guard.rs`, which
   fails the build when a persisted struct changes so a bump cannot be
   forgotten. It was forgotten once (f60fbce added `Entity.trust_tier` with no
   bump), which is why two mutually incompatible layouts both call themselves
   version 10.

Still true: nothing is *sniffed* into a different version, and an unreadable
version is refused by name rather than guessed at. Reading an old store
converts it in memory only; `kern migrate` is what rewrites it on disk.

Exception: tolerant RPC decode in `src/transport_kern_rpc_dto.rs` stays — it
serves the live attach → detect-stale → auto-restart handshake with an
already-running daemon from an older build, not persisted-data compat.

## Memory (kern)

- At task start: call kern `query` with the task topic to recall prior
  decisions, preferences, and facts before deciding anything.
- At task end, and whenever a durable decision, preference, constraint, or
  hard-won fact emerges: call kern `ingest` with ONE self-contained statement
  per fact. Include the why on decisions.
- When recall returns something wrong or stale: call `degrade` with the query
  id so it stops surfacing.
