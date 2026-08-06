# CRDTs for Federated kern State

Ticket: `A01F62NW` — "Study CRDTs for conflict-free federated kern state"

> **Implementation status (2026-07).** Stages 0/1 shipped: `src/crdt.rs` is
> `GCounter` plus an `lww_wins` tiebreak and nothing else — **no `PnCounter` was
> ever built** — `access_count` and `traversal_count` are GCounters, and
> `src/base/merge.rs` is the content-addressed entity/edge join (counters join,
> heat and timestamps merge, status follows the `Active < Superseded` lattice).
> Three deviations from this study, all deliberate: confidence is
> **replica-local**, never merged from a peer (anti-poisoning); LWW landed as
> inline lamport-stamped fields on `Entity` rather than as a named
> `LWWRegister` type; and **OR-Set for `statements` was reversed, not deferred**
> — see the note in §6 below. `Delta` has a live sender
> (`gossip::handler::start_delta_flush` at `src/gossip/handler.rs:222`, wired at
> `src/commands.rs:1082`), and inbound deltas are still clamped on receipt
> (`GOSSIP_CRDT_DELTA_MAX`, `src/gossip/handler.rs:407`). Anti-entropy was not built
> (`ROADMAP.md` — "Anti-entropy"). Type and path names below predate the
> `Thought`→`Entity` rename and the move from `crates/` to `src/`.

## 1. Problem

kern federates through hand-rolled TCP gossip (`crates/gossip`). The current wire
protocol propagates *events* (sphere advertisements, questions, pulses, peer
exchange, fetch requests) but has no formal merge semantics for the
*replicated state* those events imply — access counts, reason weights,
dedup timestamps, thought existence. Concurrent writes on partitioned
replicas therefore rely on last-writer-wins by wall-clock, or, worse,
silently diverge.

The crowd-convergence thesis requires **strong eventual consistency (SEC)**:
any two replicas that have seen the same set of updates converge to the
same state, regardless of ordering, without coordination. This is exactly
what CRDTs provide.

## 2. State inventory

Fields that are federated or federation-relevant, grouped by mutation pattern.

### 2.1 `base::types::Thought`

| Field                | Type                  | Mutation pattern                                     |
|----------------------|-----------------------|------------------------------------------------------|
| `id`                 | `String` (content hash) | Immutable; derived from content.                   |
| `external_id`        | `String`              | Set once at ingest; immutable thereafter.            |
| `kind`               | `ThoughtKind`         | Transitions `Normal → Superseded` (monotone).        |
| `superseded_by`      | `String`              | Set-once, monotone.                                  |
| `statements`         | `Vec<String>`         | Appended on dedup merge.                             |
| `vector`             | `Vec<f64>`            | Replaced on re-embed; wall-clock LWW.                |
| `gnn_vector`         | `Vec<f64>`            | Local; recomputed by GNN — **not federated**.        |
| `score`              | `f64`                 | Transient retrieval artefact — **not federated**.    |
| `access_count`       | `i32`                 | Monotonically increments (reads).                    |
| `accessed_at`        | `Option<SystemTime>`  | Max-merge (most recent wins).                        |
| `updated_at`         | `Option<SystemTime>`  | Max-merge; set when dedup merges touch the thought.  |
| `unlinked_count`     | `i32`                 | Orphan counter; increments then resets on relink.    |
| `valid_until` / `valid_at` | `Option<SystemTime>` | Authoritative window; LWW by producer.         |
| `valid_from` / `valid_to` / `invalidated_at` | `Option<SystemTime>` | Bi-temporal stamps — **node-local, not federated** (`#[serde(skip)]`). See note below. |

> **Bi-temporal fields are node-local.** `valid_from`/`valid_to`/`invalidated_at`
> are `#[serde(skip)]`, so they never enter the gossip `EntitySyncPayload` (nor the
> cold-tier blob) — the primary LMDB store persists them out-of-band via the
> `StoredKern` temporal side-map. This is deliberate: it keeps the entity's wire
> layout byte-identical to pre-temporal peers, so mixed-version nodes interoperate
> with **no protocol version bump**. The essential "this claim was invalidated"
> signal still propagates, because `superseded_by` and the `Superseded` status ARE
> gossiped (rows above); only the exact invalidation *timestamps* stay local, and
> each node re-derives its own bi-temporal view.

### 2.2 `base::types::Reason`

| Field                | Type      | Mutation pattern                                    |
|----------------------|-----------|-----------------------------------------------------|
| `id`, `from`, `to`   | `String`  | Immutable.                                          |
| `kind`               | `ReasonKind` | Set once.                                        |
| `text`               | `String`  | Set-once on enrichment.                             |
| `vector`             | `Vec<f64>`| Replaced with `text` set.                           |
| `score`              | `f64`     | Re-scored every `REFINE_INTERVAL` traversals — concurrent updates race. |
| `traversal_count`    | `i32`     | Monotonically increments.                           |

### 2.3 `gossip::ledger::Ledger`

Pure local cache of routing hints with TTL. **Not federated replicated state** —
no CRDT needed.

### 2.4 `gossip::seen::SeenSet`

Local replay-protection ring buffer. **Not federated.**

## 3. CRDT primitive mapping

For each field we name the minimal CRDT primitive that preserves its
semantics without coordination.

| kern field                   | CRDT primitive                           | Rationale |
|------------------------------|------------------------------------------|-----------|
| `Thought.access_count`       | **G-Counter** (per-replica `HashMap<NetworkId, u32>`; value = sum) | Only ever increments; each replica owns one slot; merge = pointwise max. |
| `Reason.traversal_count`     | **G-Counter**                            | Same shape as above.                                    |
| `Thought.unlinked_count`     | **PN-Counter**                           | Can increment on orphan, decrement on relink; needs +/- state vectors. |
| `Thought.accessed_at`        | **LWW-Register** keyed on `SystemTime`   | Interpretation is "most recent read"; LWW is exact fit. |
| `Thought.updated_at`         | **LWW-Register**                         | Records the merge event; ties broken by `(timestamp, origin_id)`. |
| `Thought.valid_until`/`valid_at` | **LWW-Register** with producer tiebreak | Temporal validity is authoritative per producer; LWW-by-`(timestamp, producer_id)` is safe. |
| `Thought.kind` transition to `Superseded` + `superseded_by` | **2P-Set semantic** via **monotone flag** | Transition is one-way; represent as "tombstone-ish" flag; never reverts. |
| `Thought.statements` (append-on-dedup) | **OR-Set<(text_hash, replica_id, lamport)>** | Deduped statement set with concurrent-add / remove-by-tag semantics. |
| `Reason.score`               | **LWW-Register with Lamport clock**      | Score is LLM-rated; newest rating by Lamport timestamp wins; ties by `producer_id`. |
| `Reason.text` / `Reason.vector` | **LWW-Register** (set-once-then-stable) | First non-empty write dominates; LWW is strictly cautious. |
| Thought existence (presence in `Kern.thoughts`) | **OR-Set** (id × add-tag) keyed by `content_hash` | Dedup key is `content_hash`, concurrent ingests collapse to a single add. |
| Reason existence (`by_from` / `by_to`) | **OR-Set** keyed by reason `id` | Edge identity = `id`; OR-Set gives concurrent-add idempotency. |
| Dedup near-duplicate merge   | **Two-phase**: OR-Set for presence + LWW-Register for `updated_at` + G-Counter for `access_count` | Merge is compositional on the thought's sub-fields. |
| `Kern.purpose_text` / `purpose_vec` | **LWW-Register**                  | Operator-set; single writer semantics per kern root.   |
| Peer list (`PeerExchangePayload`) | **2P-Set** (or **OR-Set** if we later need explicit "removed") | Peers only grow in practice; 2P-Set with tombstones handles intentional evict. |

### 3.1 Why not δ-CRDTs for everything?

State-based CRDTs transmit full state; operation-based need exactly-once
delivery. **δ-CRDTs** ship only the *delta* of the state that changed
since last sync — ideal for gossip because:

- Deltas compose associatively / commutatively / idempotently.
- The gossip layer already deduplicates by message id (`SeenSet`), so
  at-least-once delivery is fine.
- Bandwidth scales with *change rate*, not graph size.

Recommendation: model every field above as a **δ-CRDT variant** of its
named primitive. The primitive (G-Counter, OR-Set…) defines the lattice;
the δ-form defines the wire payload.

## 4. Current ad-hoc merge issues resolved

Mapping the catalogue of concrete, known race conditions to the above.

| Current behaviour                                                         | CRDT fix                          |
|---------------------------------------------------------------------------|-----------------------------------|
| `retrieval::score::commit_access` increments `access_count` locally; two replicas answering the same query concurrently each +1 — one gets overwritten on next gossip. | G-Counter: both +1s survive; sum converges. |
| `refine_edges` sets `Reason.score` to LLM rating; two replicas refining the same edge produce two writes; current code does a raw `r.score = clamped` — last gossip wins, but "last" is arbitrary. | LWW-Register keyed on `(lamport, producer_id)`. |
| Concurrent ingest of the same content at two replicas produces two `Thought`s with the same `content_hash`; `accept::accept` dedups locally but the *other* replica does not see the dedup until it gossips — both live briefly. | OR-Set keyed on `content_hash`: concurrent adds collapse idempotently. |
| `unlinked_count` is incremented by the tick/decay worker; concurrent decay on two replicas double-counts. | PN-Counter: increment/decrement vectors. |
| `statements` vec is appended on dedup merge; if two replicas dedup the same incoming text concurrently the append duplicates. | OR-Set of `(text_hash, origin)`. |
| `valid_until` currently relies on producer-local wall clock with no conflict rule — two producers racing to set validity on the same thought silently overwrite. | LWW with `(producer_id, lamport)` tiebreak documented in schema. |
| Ledger routing hints can flap if two peers claim the same `kern_id`. Not a CRDT case — TTL is correct here. | (Keep as-is; local cache, not replicated state.) |

## 5. Mapping onto `crates/gossip` wire format

Today the wire is a single `GossipMessage` envelope with a tagged `GossipPayload`
union (`Sphere`, `Question`, `Pulse`, `PeerExchange`, `Fetch`, `FetchResult`).

### 5.1 Minimal migration — additive payload variants

Add two new `GossipKind` discriminants; leave all existing variants
unchanged for backward compatibility:

```rust
pub enum GossipKind {
  Sphere = 0,
  Question = 1,
  Pulse = 2,
  PeerExchange = 3,
  Fetch = 4,
  Delta = 5,        // NEW: δ-CRDT payload
  AntiEntropy = 6,  // NEW: full-state merge (rare, used on rejoin)
}

pub enum GossipPayload {
  // ... existing ...
  Delta(DeltaPayload),
  AntiEntropy(AntiEntropyPayload),
}

pub struct DeltaPayload {
  pub kern_id:   String,
  pub object_id: String,        // thought_id or reason_id
  pub field:     CrdtField,     // enum: AccessCount, Score, Statements, ...
  pub delta:     CrdtDelta,     // serialised δ (G-Counter / OR-Set / LWW)
  pub lamport:   u64,           // per-replica logical clock
  pub origin:    String,        // replica id = NetworkID
}

pub enum CrdtDelta {
  GCounterInc { replica: String, by: u64 },
  PnCounter   { replica: String, pos: u64, neg: u64 },
  LwwSet      { value: Vec<u8>, ts: u64, origin: String },
  OrSetAdd    { key: String, tag: (String, u64) },
  OrSetRemove { key: String, tags: Vec<(String, u64)> },
}
```

The existing `SeenSet` continues to dedupe redundant deltas by `GossipMessage.id`.
Anti-entropy is a targeted pull: a joining node asks an established peer for the
full CRDT state of a `kern_id`, applies it (merge is idempotent), then joins the
delta stream.

### 5.2 Per-thought wire representation

Inside persisted `Thought` we replace raw fields with their CRDT
"shadow" state, serialised via `bincode` like everything else:

```rust
pub struct Thought {
  pub id: String,
  // ... immutable fields ...
  pub access_count:  GCounter,     // was i32
  pub unlinked_count: PnCounter,   // was i32
  pub accessed_at:   LwwRegister<SystemTime>,
  pub updated_at:    LwwRegister<SystemTime>,
  pub valid_window:  LwwRegister<(Option<SystemTime>, Option<SystemTime>)>,
  pub statements:    OrSet<String>,
  // ...
}
```

Hot-path readers (scoring, retrieval) read *materialised* scalars via
cheap accessors: `fn access_count(&self) -> i32 { self.access_count.value() }`.

### 5.3 Lamport clocks

Each node keeps a single `AtomicU64` logical clock, bumped on every
local mutation and on every incoming delta (`max(local, remote) + 1`).
The clock travels in every `DeltaPayload` and is the tiebreak in all
LWW registers. This replaces the current implicit wall-clock LWW.

## 6. How the migration decomposes

The analysis broke the change into five independently shippable, independently
reversible stages, recorded here as the decomposition — what each stage is and
what it depends on. What is scheduled, and in what order, is `ROADMAP.md`'s;
where a stage did not survive contact with the code, that is noted on the stage.

### Stage 0 — foundations (est. 1–2 days)

- Add a tiny `crates/crdt/` utility crate (G-Counter, PN-Counter,
  LWW-Register, OR-Set) — hand-rolled, matches the self-contained
  philosophy. No external crate.
- Inline unit tests per file (project convention).
- Pure functions; `merge(&mut self, &Other)` returning `bool` for
  "state changed".

### Stage 1 — shadow counters (est. 1 day)

- Internally mirror `access_count` and `traversal_count` into a
  G-Counter held in a side-map keyed by thought/reason id.
- Gossip increments as `Delta{GCounterInc}` — cohabits with existing
  message kinds.
- Materialised `i32` remains the read path. Verify convergence in
  integration tests under simulated partition.

### Stage 2 — full CRDT thought/reason (est. 2–4 days)

- Replace raw fields with CRDT-typed fields in `base::types`. `bincode`
  schema bumps a version tag; add a one-shot persist migration.
- `accept::accept` and dedup path write through the CRDT types.
- Remove ad-hoc last-writer logic in `refine_edges`.

> **Reversed in part.** The OR-Set-for-`statements` half of this stage is not
> pending — it was refused on inspection. `id == content_hash(text)` and
> `statements == [text]`, so a same-id peer already holds identical content and a
> differing one is asserting content its id does not hash to; importing it would
> break content-addressing and resurrect locally-cleared statements. `merge_entity`
> never imports them (`src/base/merge.rs:112`) and `CrdtTarget::Statements` is
> rejected on receipt (`src/gossip/handler.rs:502`), kept as a refused variant so a
> peer on an older build cannot inject text under a content-addressed id.
> Statements converge through full EntitySync bodies instead. The LWW half landed
> as inline lamport-stamped fields, not as CRDT-typed ones.

### Stage 3 — anti-entropy (est. 2 days)

- Periodic pull: pick a random peer, request `AntiEntropy` for one
  `kern_id`, merge. Uses existing `Fetch` primitive for request framing.
- Exponential backoff if divergence stays after N rounds (indicates
  partition).

### Stage 4 — remove redundant merge code (est. 0.5 day)

- Delete the hand-rolled conflict resolution in ingest/retrieval.
- All merges go through CRDT `merge()`; ordering and delivery semantics
  now formally match SEC.

## 7. Risks and non-goals

- **Garbage collection**: OR-Set tombstones and LWW histories grow
  unboundedly. The analysed remedy is time-bounded compaction using
  `valid_until` / access recency as cues (`ROADMAP.md` — "CRDT growth and
  re-embedding across replicas").
- **GNN weights** (`gnn_weights: Vec<u8>`): model parameters are
  *not* replicated state — they are local derivations. Keep excluded.
- **Vectors** (`Thought.vector`, `Reason.vector`): LWW is coarse; if
  we later re-embed at two replicas with different models we need a
  richer scheme. Out of scope for this study.
- **Security / Byzantine**: CRDTs give SEC only in a *non-Byzantine*
  model. A malicious peer can poison counters. Signing deltas with
  `producer_id` public keys is future work and orthogonal.
- **Clock skew**: LWW tiebreak uses Lamport clock, not wall clock, so
  skew cannot cause lost updates — only ordering ambiguity, which CRDT
  convergence tolerates.

## 8. Decision

**Adopt δ-CRDTs field-by-field, prioritised by risk.** Start with
G-Counter for `access_count` / `traversal_count` — highest race
probability, simplest CRDT, zero semantic change. Then OR-Set for
dedup/statements, then LWW for timestamps and scores. The existing
`GossipMessage` envelope absorbs the change additively; no breaking
wire-format bump is required.

> **Amended by implementation.** The OR-Set-for-`statements` leg of this ordering
> is void — content-addressed ids make importing remote statement text both
> unsound and a resurrection vector, so it is refused on both ends (§6, Stage 2).
> OR-Set for the dedup path and LWW for timestamps/scores stand; LWW shipped as
> inline lamport-stamped fields rather than a named register type.

## 9. References

- Shapiro et al., *Conflict-free Replicated Data Types*, INRIA 2011.
- Almeida, Shoker, Baquero, *Delta State Replicated Data Types*, 2018.
- Project files grounding this design:
  - `crates/base/src/types.rs` — `Thought`, `Reason` field set.
  - `crates/gossip/src/types.rs` — current wire envelope.
  - `crates/gossip/src/handler.rs` — current merge dispatch.
  - `crates/gossip/src/seen.rs` — existing message dedup (kept as-is).
  - `crates/ingest/src/lib.rs` — `dedup_threshold` and `find_duplicate`.
  - `crates/retrieval/src/score.rs` — `commit_access` race site.
  - `crates/retrieval/src/answer.rs` — `refine_edges` LWW race site.
