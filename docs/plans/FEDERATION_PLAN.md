# Federation Plan — ring topology, contracts, delegates for kern

Status: **implemented (v0), 2026-07-27** — all six phase gates green. Drafted
2026-07-26. Deviations from this spec are listed at the bottom
("Implementation notes"); the section texts below are kept as written.

Provenance: kern's federation design predates our awareness of Freenet. The
existing gossip layer already converged on the same core ideas independently —
content-hash self-certifying entity IDs (`id_matches_body`), commutative
CRDT merge (`merge.rs`), untrusted-signal stripping, peer rate limiting.
Freenet is not the origin of this architecture; it is external validation of
the direction we had already taken, and we use it as inspiration for the parts
we had not yet specced (key-as-policy contracts, small-world ring routing,
subscription trees, delegate secret isolation) on top of how we started to
architect the federation.

Legal basis: clean-room reimplementation of Freenet concepts (small-world ring,
contract-keyed state, summary-delta sync, delegate secret isolation). No code
copied from freenet-core (AGPL-3.0) or freenet-stdlib (LGPL-3.0) — kern is MIT
and stays MIT. Sources allowed: freenet.org/build/manual (documentation),
Kleinberg small-world papers, public CRDT literature.

---

## 0. What exists today (baseline)

- `src/gossip/node.rs` — flat TCP peer list, capped, broadcast + fetch.
- `src/gossip/discovery.rs` — UDP LAN announce.
- `src/gossip/handler.rs` — entity sync into phantom kerns, `id_matches_body`
  (content-hash self-certifying entity IDs), question/answer, CRDT delta.
- `src/crdt.rs` — GCounter, `lww_wins` (lamport, producer tiebreak).
- `src/base/merge.rs` — commutative/idempotent entity merge,
  `strip_untrusted_ranking_signals`, remote cap.
- `src/gossip/rate.rs`, `seen.rs` — per-peer rate limits, dedup TTL set.
- Trust model: shared `network_id` string = full mutual trust. This is the
  thing the plan replaces.
- Auth to local daemon: `mcp-token` file, owner-only (`config/serve.rs`).

Keep all of it. The plan layers identity, topology, and per-key policy on top;
`merge.rs` becomes the default contract's `update_state`.

---

## 1. Identity (peer keys)

New module `src/gossip/identity.rs`.

- Keypair: ed25519 (crate: `ed25519-dalek`, MIT/Apache — license-clean).
- Minted at first daemon boot, path `.kern/state/peer.key`, owner-only perms,
  same minting pattern as `mint_token` in `config/serve.rs`.
- `PeerId = blake3(pubkey)` (32 bytes). Display: hex, `short_id` truncation.
- Ring location: `loc(PeerId) = u64::from_le_bytes(id[..8]) as f64 / 2^64`,
  giving a point on the circle `[0, 1)`.
- Every wire frame gains an envelope: `{ from: PeerId, sig: Sig, body }`.
  Signature over `blake3(body || lamport)`. Verify on receive; drop invalid
  before rate-limit spend (invalid sig = free-to-send, so verification must be
  cheap and precede any allocation of per-peer state).

## 2. Circular view — small-world ring topology

New module `src/gossip/ring.rs`. Replaces flat `Node.add_peer` list for WAN;
UDP LAN discovery stays as a bootstrap source.

Data:

```rust
struct RingView {
  near: Vec<PeerEntry>,      // k nearest ring neighbors each side, k = 4
  far: Vec<PeerEntry>,       // long links, target count = 8
  loc: f64,                  // own location
}
struct PeerEntry { id: PeerId, addr: SocketAddr, loc: f64, last_seen: u64 }
```

Rules:

- Ring distance: `d(a,b) = min(|a-b|, 1-|a-b|)`.
- Long links sampled with density ~ `1/d` (Kleinberg exponent 1 for a 1-D
  ring) — this is what makes greedy routing O(log² n).
- Join: connect to any bootstrap peer (config seed or LAN discovery), issue
  `FindNearest(self.loc)`; each hop forwards greedily to its neighbor closest
  to the target; terminal peer returns its `near` set. Adopt, connect, announce.
- Maintenance: piggyback peer exchange on existing heartbeat
  (`Node.start_heartbeat`); evict entries not seen for TTL; keep `near`
  correct before `far` (correctness of greedy routing depends only on `near`).
- Greedy route primitive:

```rust
fn route(&self, target: f64) -> Option<&PeerEntry> // strictly-closer neighbor, else None (we are terminal)
```

- Cap total connections at existing peer cap; `near` has absolute priority.

## 3. Contracts — the authentication method

New module `src/gossip/contract.rs`.

Key idea (Freenet's): the key IS the policy. A shared kern is addressed by the
hash of its validation policy + parameters, so a peer holding the key knows
exactly what writes are admissible, and no authority is needed.

```rust
trait SyncContract: Send + Sync {
  fn validate_delta(&self, params: &Params, delta: &Delta) -> Result<(), Refusal>;
  fn summarize(&self, state: &KernState) -> Summary;      // compact digest
  fn diff(&self, state: &KernState, remote: &Summary) -> Delta;
  fn apply(&self, state: &mut KernState, delta: Delta) -> Applied; // must be commutative + idempotent
}
```

- `ContractId = blake3(contract_kind_tag || canonical_params_bytes)`.
  Ring location of the shared kern = `loc(ContractId)`.
- `Params` v0 (serde, canonical bincode for hashing):

```rust
struct ParamsV0 {
  owners: Vec<PubKey>,          // may sign anything
  writers: WritePolicy,         // Open | Allowlist(Vec<PubKey>) | OwnersOnly
  kinds: Option<Vec<EntityKind>>, // admissible claim kinds, None = all
  max_entities: u32,            // hard cap, replaces remote_cap for this kern
  retention_secs: Option<u64>,  // forced TTL on every entity
  private: Option<PrivacyV0>,   // see §6
}
```

- Builtin contract v0 `SignedCrdt`:
  - `validate_delta`: every entity body must (a) satisfy `id_matches_body`,
    (b) carry a signature by an admissible writer, (c) match `kinds`,
    (d) fit under `max_entities`. Refusals are counted, never panic.
  - `apply`: existing `merge_entity` + `strip_untrusted_ranking_signals` +
    lamport joins. Nothing new to write — this is `merge.rs` behind the trait.
  - `summarize`: sorted `(entity_id, lamport)` pairs hashed into a 3-level
    merkle-ish digest (prefix buckets on id bytes) so `diff` transfers only
    missing/stale ids, then bodies. Reuse `flushed_epoch` plumbing where it fits.
- Later (phase gated behind existing `plugins` feature): wasm contracts via
  extism, same seam as intake transcoders. `contract_kind_tag` for wasm =
  hash of the wasm module, exactly Freenet's key=code trick. NOT in v0.
- Migration: `network_id` mode removed (alpha, no compat). Old clusters

## 4. Subscription trees + delta sync

Extend `handler.rs`.

- `Subscribe(ContractId)`: routed greedily toward `loc(ContractId)`. Each hop
  records `(contract_id, downstream_peer)` in a subscription table (bounded,
  LRU). The peer closest to the key is the tree root. Result: subscribers form
  a tree along routing paths — Freenet's propagation structure.
- Update flow: local ingest into a shared kern produces a signed delta;
  send to upstream + all downstream subscribers; each receiver runs
  `validate_delta` then `apply`, forwards only if `Applied::Changed`
  (natural flood suppression; `seen.rs` backstops cycles).
- Anti-entropy: on subscribe and every `sync_interval_secs` (default 300),
  exchange `summarize` with tree parent, transfer `diff` both directions.
  Replaces the current periodic full entity-sync walk for contract kerns.
- Rate limiting: existing `RateLimiter` keyed by PeerId instead of addr.

## 5. Delegates — secret isolation

Cheap version, no wasm needed:

- The daemon is the delegate. Peer key never leaves the daemon process.
- New RPC on the kern socket (mcp-token gated, same auth as everything):
  - `sign { payload_hash }` — returns signature by peer key.
  - `contract_grant { contract_id, pubkey }` — owner-signed params amendment
    (this bumps ContractId; see upgrade note below).
- Agents/CLI/MCP callers never read key files; they ask the daemon.
- Upgrade note: amending params changes the ContractId (key = policy hash, so
  policy changes move the key). V0 answer: publish a signed `Tombstone { new_id }`
  entity in the old contract; subscribers follow it once, then unsubscribe old.
  Matches Freenet's "upgrading contracts" shape without their machinery.

## 6. Private shared kerns

`PrivacyV0` in params:

```rust
struct PrivacyV0 { scheme: u8 /* 0 = xchacha20poly1305 */, key_hint: [u8; 8] }
```

- Entity text/vector encrypted client-side before ingest into the shared kern;
  symmetric key distributed out-of-band (not this plan's problem, v0 = file).
- Contract validates signatures + ids over ciphertext — relay peers store and
  route bytes they cannot read. Local daemon decrypts on merge into the
  phantom kern so retrieval/embedding sees plaintext locally only.
- Consequence: remote peers cannot dedup/semantic-route encrypted entities;
  they hold them as opaque rows. Acceptable — that is the price of private.

## 7. Config

`GossipConfig` additions (defaults keep everything off, like today):

```toml
[gossip]
enabled = false
ring = false                 # phase 2 switch; false = legacy flat peers
identity_path = ""          # default .kern/state/peer.key
sync_interval_secs = 300
subscriptions = []           # contract ids to subscribe on boot

[[gossip.contracts]]         # contracts this node hosts/owns
kind = "signed-crdt-v0"
owners = ["ed25519:..."]
writers = "owners-only"
```

## 8. Wire frames (new/changed)

All framed like existing transport (`gossip/transport.rs`), max-size capped,
envelope-signed per §1:

- `FindNearest { target: f64 }` / `Nearest { peers: Vec<PeerEntry> }`
- `Subscribe { contract: ContractId }` / `SubAck { summary: Summary }`
- `Delta { contract: ContractId, delta: Delta, lamport: u64 }`
- `SyncSummary { contract: ContractId, summary: Summary }`
- `SyncDiff { contract: ContractId, delta: Delta }`
- `Tombstone { contract: ContractId, new_id: ContractId, sig: Sig }`
- Legacy frames (pulse, question/answer, entity-sync, crdt-delta) unchanged
  inside legacy mode; retired for contract kerns once phase 4 lands.

## 9. Phasing + test gates

Each phase lands independently, feature-flagged, alpha rules (no migrations).

1. **Identity + signed envelopes** — unit: sig verify precedes rate-limit
   spend; wrong-key frame dropped and counted; key file owner-only
   (mirror `open_private_append` tests).
2. **RingView + greedy routing** — sim test (no sockets): 1k synthetic peers,
   assert greedy route reaches nearest-to-target in ≤ O(log² n) hops for 99%
   of random targets; churn test: kill 20% peers, `near` repairs, routing
   still terminates.
3. **SignedCrdt contract** — property tests: apply is commutative/idempotent
   (reuse `merge_entity_is_order_independent_and_idempotent` shape);
   validate refuses wrong signer, wrong kind, over-cap, forged id; summarize/
   diff round-trip: two divergent states converge byte-identical after one
   exchange each direction.
4. **Subscription tree + delta propagation** — e2e (tests/e2e/ harness):
   3 daemons chained, ingest at leaf, assert entity queryable at all 3;
   partition one, heal, anti-entropy converges within one sync interval.
5. **Delegate RPC** — sign endpoint refuses without token; key unreadable by
   caller uid test where platform allows.
6. **Privacy** — encrypted entity round-trips through a relay that asserts it
   never saw plaintext (grep its store bytes for a sentinel).

## 10. Non-goals (v0)

- Wasm contracts (seam reserved, extism, later).
- Incentives/anti-spam beyond rate limits + max_entities.
- NAT traversal (assume reachable addrs or LAN; Freenet's transport tricks
  are out of scope).
- Global anonymous routing — kern federates knowledge between consenting
  peers; it is not censorship-resistant publishing.

---

## Implementation notes (2026-07-27)

Where the code lives and where it deliberately deviates:

- **§1 Identity** — `src/gossip/identity.rs`. As specced. Envelope =
  `SignedFrame` in `types.rs`; verification inside `transport::decode_msg`,
  structurally before the seen-set/peer-list/rate-limit in
  `Node::handle_conn`. The Question budget keys on the verified PeerId.
- **§2 Ring** — `src/gossip/ring.rs`. Deviation: the join walk is
  **iterative (requester-driven)**, not recursive hop-forwarding — each hop
  answers `Nearest{own + near + far}` and holds no request state; same
  greedy result, simpler failure story. Both §9 gates pass (99% nearest-peer
  reach in ≤ log²n hops at n=1000; 20% churn).
- **§3 Contracts** — `src/gossip/contract.rs`. Deviation: `Summary` carries
  its sorted `(id, lamport)` entries **alongside** the 16 nibble-bucket
  hashes, so `diff` names missing/stale ids in one round trip instead of
  per-bucket fetches; matched buckets are still skipped. Acceptable at v0
  sizes (`max_entities` caps the list); a fetch-per-bucket protocol can
  replace it wire-compatibly later. The one migration mapping is removed
  (alpha, no compat).
- **§4 Subscriptions** — `src/gossip/subs.rs` + `handler.rs`
  (`handle_subscribe`/`handle_suback`/`handle_contract_delta`/
  `handle_sync_summary`, `start_contract_sync`, `publish_to_contract`).
  Two guards the spec did not name, added after the three-node gate flaked:
  **first parent wins** (a raced SubAck cannot re-root a node) and **a
  downstream peer is never adopted as upstream** — without them a raced tree
  extension could close a b↔c cycle that starves the root. Only hosts (nodes
  carrying the params) join a tree: every hop validates, and validation
  needs the params. The §9 gate runs three real-socket nodes in-process
  rather than three spawned daemons; the daemon wiring itself is exercised
  by boot config (`[gossip] subscriptions`, `[[gossip.contracts]]`).
- **§5 Delegates** — `src/mcp/tools_delegate.rs` (`sign`, `contract_grant`),
  token-gated like every mcp tool. `contract_grant` is stateless: it takes
  the current contract table, checks the daemon's key is an owner, returns
  amended params + new ContractId + tombstone signature
  (`tombstone_digest`). `handle_tombstone` verifies the owner signature and
  unsubscribes; following the pointer is the operator's call in v0.
- **§6 Privacy** — `src/gossip/privacy.rs` (xchacha20poly1305). Deviation
  worth naming: the entity **vector is dropped**, not encrypted — an
  embedding of hidden text is a leak of it, and a relay cannot route what it
  cannot read (the spec's own consequence note). The sealed id is the
  ciphertext's content hash, so `id_matches_body` and writer signatures hold
  on relays; the key holder restores the plaintext hash locally.
- **Known v0 limits** — the signed-body cache (`ContractState`) is
  in-memory: after a restart a node cannot re-prove foreign bodies until a
  peer re-serves them. Ingest-side routing into a shared kern is the
  `publish_to_contract` seam, not yet an `[ingest]` config path. Both are
  follow-ups, not spec changes.
