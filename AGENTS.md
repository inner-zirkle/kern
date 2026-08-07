# kern — agent notes

Read `docs/windmill/ORACLE.md` before acting.

## Alpha — no compatibility

kern is version alpha. Features we change need **no** backward compatibility:
no migration paths, no legacy decode fallbacks, no serde aliases for renamed
fields, no wire-format stability across builds. When a persisted or wire format
changes, bump the single live format version (`FORMAT_VERSION` in
`src/base_store.rs`, `WEIGHT_FILE_VERSION` in `src/gnn_persist.rs`) so old data
is rejected cleanly — never migrated, never sniffed. Old stores are wiped and
reingested.

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
