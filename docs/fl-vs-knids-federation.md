# Federated Learning vs kern Federation

Ticket: `246BPAA1` — "Study federated learning parallels vs kern federation"

> **Implementation status (2026-07).** Comparison study; the decisions stand.
> Of the "steal" list in §8, clipping shipped as the inbound CRDT delta clamp
> (`GOSSIP_CRDT_DELTA_MAX`); per-peer ingest budgets, trimmed-mean
> materialisation, and the provenance ledger extension were not built. Code
> paths below reference the pre-1.0 `crates/` layout.

## 1. Problem framing

kern federates **data** (thoughts, reasons, pulses, sphere advertisements)
between nodes using hand-rolled TCP gossip (`crates/gossip`). Federated
Learning (FL) federates **gradients** (model updates) between clients and
a coordinator. Both systems share a core constraint — partial observation,
no shared memory, unreliable links — and a core goal — let independent
replicas benefit from each other's experience. That shared DNA makes FL
the nearest peer in the literature even though the unit of exchange is
different.

This document answers three questions:

1. Which FL guarantees transfer to kern's data-level federation?
2. Where does the analogy break, and why?
3. What primitives should we steal verbatim vs deliberately reject?

It complements the CRDT study (`docs/crdts-federation.md`), which settles
*how* state merges, and the PageRank study
(`docs/pagerank-authority.md`), which settles *whose* state is trusted.
FL settles a third orthogonal axis: *what can a peer learn from a peer
without exposing its raw data*.

## 2. Federated Learning in one page

Three canonical algorithms frame the field.

### 2.1 FedAvg (McMahan et al., 2017)

The reference algorithm. A coordinator broadcasts global model weights
`w_t`; each client `k` runs `E` local SGD epochs on its private dataset
`D_k` producing `w_{t+1}^k`; coordinator computes the weighted average:

```
w_{t+1} = Σ_k (|D_k| / |D|) · w_{t+1}^k
```

No raw data leaves the client. Only the **delta** (gradient / weight
update) crosses the wire. Convergence requires the coordinator plus
periodic synchronous rounds.

### 2.2 Differential Privacy (DP-FedAvg, DP-SGD)

Before sending `w_{t+1}^k`, the client:

1. Clips the update to bound its L2 norm (`||Δ|| ≤ C`).
2. Adds calibrated Gaussian / Laplace noise (`Δ + N(0, σ²C²)`).

This gives an `(ε, δ)`-DP guarantee: the presence or absence of any
single training example changes the output distribution by at most a
bounded factor. The coordinator spends a **privacy budget** per round,
tracked by a moments accountant — once exhausted, training halts.

### 2.3 Secure aggregation (Bonawitz et al., 2017)

A cryptographic protocol that lets the coordinator compute
`Σ_k Δ_k` without seeing any individual `Δ_k`. Clients pairwise-mask
their updates with shared seeds; masks cancel in the sum. Adds one
round of dropout-tolerant key exchange per aggregation round.

### 2.4 Implicit assumptions

- A trusted (or at least functionally-trusted) **coordinator**.
- **Synchronous rounds**: clients train against a known global state.
- Clients may be **Byzantine** — DP + byzantine-robust aggregation
  (Krum, median, trimmed mean) is active research.
- The **model architecture is shared** and fixed.

## 3. kern federation in one page

From `crates/gossip`:

- **Transport**: length-prefixed bincode over raw TCP; no coordinator.
- **Payloads**: `Sphere` (scope advertisement), `Question` (cross-node
  reason), `Pulse` (liveness/urgency), `PeerExchange` (membership),
  `Fetch`/`FetchResult` (pull a thought or reason by id).
- **Delivery**: best-effort gossip, deduped by `SeenSet` (replay ring).
- **Merge semantics**: today ad-hoc last-writer-wins; CRDT study
  proposes δ-CRDTs per field.
- **Unit of exchange**: the *artefact itself* — thought text, reason
  edge, purpose vector. The data is the payload.

There is no central aggregator, no training round, no model delta. Every
node holds a full (subset of) graph and answers queries locally. Peers
exist to (a) expand recall beyond a single node's ingest and (b) let
agents hand off context between kern-backed agent sessions.

## 4. Analogy map

| FL concept                          | kern analogue                                   | Transfer strength |
|-------------------------------------|-------------------------------------------------|-------------------|
| Global model `w_t`                  | Replicated graph state (CRDT lattice)           | Strong            |
| Client local dataset `D_k`          | Node's locally-ingested thoughts & reasons     | Strong            |
| Local SGD update `Δ_k`              | Ingest event / dedup merge / access increment   | Weak — our "update" *is* the data |
| Weighted average `Σ (|D_k|/|D|) Δ_k`| CRDT merge (G-Counter sum, OR-Set union)        | Structural only — different math |
| Coordinator                         | Absent. Gossip mesh replaces it.                | Does not transfer |
| Synchronous round                   | Continuous async propagation                    | Does not transfer |
| Privacy budget `(ε, δ)`             | No analogue today; candidate primitive          | Strong            |
| Secure aggregation                  | No analogue; candidate primitive for pulses     | Partial           |
| Byzantine-robust aggregation        | Peer trust / provenance signing                 | Partial           |
| DP noise injection                  | Deliberate rejection (see §6)                    | Does not transfer |
| Moments accountant                  | Audit ledger for ingest provenance              | Partial           |
| Model compression (sketches)        | Delta compression in gossip wire                | Weak              |

## 5. Transferable primitives

Primitives worth stealing, ranked by value and fit.

### 5.1 Privacy budget per peer (adopt)

FL tracks `(ε, δ)` per client across rounds; once the budget is spent,
the client stops contributing. kern has an analogous risk: a
semantically-noisy or low-trust peer should not be able to continually
bias a local graph. **Primitive**: a per-peer ingest budget (count of
accepted thoughts per window + a similarity floor). When a peer's
incoming rate or divergence exceeds budget, down-weight future ingests
rather than block. Compose with PageRank authority scoring.

### 5.2 Secure aggregation for pulses & counters (adopt partial)

Pulses (`PulsePayload.strength`) and access counters are aggregate
signals across peers — exactly the shape secure aggregation was built
for. Pairwise masking over a gossip mesh is harder than over a star
topology, but the **Shamir-style dropout-tolerant mask** primitive
generalises. Reserve for a later phase when pulse data is considered
sensitive (e.g. cranyums sharing agent behaviour signals).

### 5.3 Clipping before aggregation (adopt)

FL clips each `||Δ_k|| ≤ C` to bound any one client's influence. kern
has no equivalent: a peer can inject an unboundedly-weighted reason or
spam access increments. **Primitive**: clip per-delta contribution size
(max `access_count` increment per peer per window; max reason-weight
delta). Cheap, no crypto, huge robustness win. Ties directly into the
G-Counter design in the CRDT study.

### 5.4 Byzantine-robust aggregation (adopt for scoring)

Replace raw sum / mean with **trimmed mean or coordinate-wise median**
for federated scalars (reason scores, decay weights). One malicious
peer cannot move the outcome by more than a bounded amount. Zero
coordinator required — each replica applies the robust rule locally
when computing materialised views from CRDT state.

### 5.5 Moments accountant → provenance ledger (adopt)

FL's privacy accountant records cumulative privacy loss. kern already
has `gossip::ledger::Ledger` for routing hints; extend with a
**provenance log** per thought: `(origin_peer, lamport, confidence)`
tuples. Enables retrospective down-weighting when a peer is later
deemed untrusted, without requiring retraction.

### 5.6 Round-based anti-entropy cadence (adopt)

FL's synchronous round is overkill, but the *periodicity* is useful.
Today gossip is purely event-driven. Add a periodic anti-entropy pull
(already in the CRDT plan Stage 3) — FL's round pacing informs the
back-off schedule: exponential jitter keyed to divergence estimate.

## 6. Deliberate non-adoptions

Primitives that look tempting but must not be imported.

### 6.1 A coordinator

Non-negotiable. kern is designed as a peer-to-peer substrate for agent
systems. Reintroducing a coordinator creates a single point of failure,
a governance question we have no answer to, and defeats the self-
organising thesis. Gossip + δ-CRDTs + PageRank give us async SEC
without one.

### 6.2 DP noise on thought content

Adding calibrated noise to thought vectors or text would destroy
retrieval quality. FL tolerates noise because gradients are
redundant — losing ε of one dimension is fine. A thought is a
*specific* artefact; noise makes it unrecognisable. Privacy at kern's
layer must come from **scope control** (who sees what spheres) not
from noise.

### 6.3 Weighted averaging of vectors across peers

Naive: "average a thought's vector across peers that hold it". Breaks
on two fronts: (a) the same thought has a deterministic embedding from
its content hash, so averaging is either a no-op or a silent bug; (b)
if peers use different embedding models, averaging is meaningless. Use
LWW keyed on (model_id, lamport) instead — already in the CRDT plan.

### 6.4 Shared model architecture

FL requires every client to run the same architecture. kern explicitly
allows heterogeneous nodes — different embedding models, different GNN
dimensions, different claim-kind sets. The GNN layer's weights are
**excluded from federation** for this exact reason (see CRDT study §7).

### 6.5 Synchronous rounds

Gossip converges without rounds; adding them re-creates the
coordinator problem (who fires the round?). Anti-entropy is periodic
but *unilateral* — each node pulls on its own schedule.

### 6.6 Gradient compression / sketches

Our deltas are already small (a counter increment, an OR-Set add). The
sketching machinery from FL targets MB-scale gradient tensors. YAGNI.

## 7. Where the analogy actually breaks

Three structural differences deserve explicit naming, so we are not
tempted by surface similarities:

1. **The delta is the payload.** In FL, `Δ_k` is a lossy, privacy-
   preserving summary of `D_k`. In kern, the gossip message *is* the
   thought. There is no compression dimension to trade off against
   privacy. Privacy has to live elsewhere — at the scope / membership
   layer.
2. **No shared loss function.** FL optimises a global objective. kern
   has no global objective; each node answers its own queries against
   its own purpose vector. "Convergence" in kern means SEC over the
   graph state, not minimisation of a loss.
3. **Time is not rounds.** FL treats time as a discrete sequence of
   rounds. kern treats time as a continuous stream of events ordered
   by Lamport clocks. Algorithms that assume round `t` (learning-rate
   schedules, privacy accountants indexed by round) need re-framing
   against Lamport / wall-clock windows.

## 8. Decision

**Steal**: clipping, per-peer privacy budgets (as ingest budgets),
byzantine-robust aggregation for scalar scores, provenance ledgers,
anti-entropy cadence informed by round-based pacing. Optionally secure
aggregation for pulses when sensitivity warrants it.

**Reject**: coordinator, DP noise on content, cross-peer vector
averaging, shared model assumption, synchronous rounds, gradient
sketching.

The stolen set has a natural dependency order, recorded here as analysis — the
schedule is `ROADMAP.md`'s ("Two FL-derived bounds adopted on paper, neither in
effect", "No Sybil defence is in effect"). Cheapest and least coupled first:

1. Per-peer ingest clip + budget — small, high value, no new crypto.
2. Trimmed-mean materialisation for CRDT scalars — piggybacks on the
   CRDT migration, so it wants that migration to exist first.
3. Provenance log extension to `gossip::ledger`.
4. Secure-aggregation pulses — dependent on a use case, not on the three
   above.

## 9. References

- McMahan et al., *Communication-Efficient Learning of Deep Networks
  from Decentralized Data*, AISTATS 2017. (FedAvg)
- Abadi et al., *Deep Learning with Differential Privacy*, CCS 2016.
  (DP-SGD / moments accountant)
- Bonawitz et al., *Practical Secure Aggregation for Privacy-Preserving
  Machine Learning*, CCS 2017.
- Blanchard et al., *Machine Learning with Adversaries: Byzantine
  Tolerant Gradient Descent*, NeurIPS 2017. (Krum)
- Yin et al., *Byzantine-Robust Distributed Learning*, ICML 2018.
  (trimmed mean, median)
- Kairouz et al., *Advances and Open Problems in Federated Learning*,
  Foundations and Trends 2021. (survey)
- Project files grounding this comparison:
  - `crates/gossip/src/types.rs` — wire envelope.
  - `crates/gossip/src/handler.rs` — merge dispatch.
  - `crates/gossip/src/ledger.rs` — routing / provenance substrate.
  - `docs/crdts-federation.md` — convergence semantics.
  - `docs/pagerank-authority.md` — peer authority scoring.
