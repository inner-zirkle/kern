# Vision

kern is a self-learning memory substrate for AI agents: one daemon per working
directory owns a knowledge graph that a caller writes durable facts into, keeps
itself small without gardening, serves the right context back at recall time,
and optionally federates across machines — local-first, self-contained,
in-process, no cloud, no query-time LLM required. Capture is never automatic;
everything after the write is. It is not a vector store you operate (chunk,
embed, index, prune); it is a process that operates itself, defined by four
autonomous properties — self-learning from what is used, structured,
self-compacting, self-distributing — each the inverse of a RAG chore you'd
otherwise own. The competitive set is agent memory (Zep/Graphiti, Mem0, Letta),
not general-purpose vector databases. kern publishes no number against that set
and has none of its own; what measures retrieval quality with no LLM in the
scoring loop now exists — `tests/e2e/test_recall.py`, the answer to what was
`ROADMAP.md`'s first question — and it steers work without certifying anything,
so the claim standard stands unchanged. Every open item lives in `ROADMAP.md`.

## The test

A change fails the vision if it breaks any of these:

- **Two caller-driven ways in, never a hidden one.** Durable facts enter by an
  agent calling MCP `ingest` (the primary path) or by dropping a transcript into
  `.kern/intake/` (the backup path, which the daemon distills). kern captures no
  session on its own — writing to either entry point is the caller's job — and an
  LLM outage on the intake path queues, never loses.
- **A graph, not a bag.** Storage is typed thoughts plus reason edges — the
  *why* connecting facts, not a similarity score — and recall can walk them.
  Ids are content hashes, so identical content is the same node everywhere.
- **Superseded, never deleted.** An updated or contradicted claim becomes
  bi-temporal history queryable `as_of` a past instant; the
  update-vs-contradiction call runs off the recall path.
- **Recall touches no LLM, ever.** The read path is graph + embeddings only —
  kern does no synthesis; the calling agent does. Offline-capable against a
  local embedder alone.
- **The hot graph stays bounded.** Decay + GC compact without intervention;
  Facts are never auto-forgotten; evictions spill to the cold tier before they
  drop.
- **Retrieval learns from use.** Access heat re-ranks what is actually used;
  `degrade` down-weights bad paths — a bad result never keeps ranking
  unpunished.
- **Fail open.** Intake and recall no-op on any error or outage; a session
  always proceeds.
- **All-internal.** No pluggable or fallback backend, no network hop on the hot
  path, dependencies deliberately minimal.
- **Federation is opt-in and coordinator-free.** Off by default; when on,
  nodes converge over gossip via the content-addressed CRDT — no central
  server.
- **One dispatch core.** Every surface (MCP, RPC, CLI, any future one) goes
  through the single `mcp::Server::call_tool` — never a second copy.
- **No quality claim without an instrument.** There is no recorded baseline —
  it was withdrawn, not superseded. The scorer that exists (`tests/e2e/test_recall.py`)
  runs against a bag-of-words embedder: it catches regressions and certifies
  nothing, so the standard is unchanged by its arrival (`ROADMAP.md` — "no
  quality claim of any kind") and kern claims no quality of any kind: not SOTA,
  not parity, not regression, not improvement. Latency stays claimable, from the
  e2e harness, multi-seed with error bars.
- **Per-cwd isolation.** One graph per directory; no cross-project
  contamination.
