# PageRank for DB Authority in Federated kern

**Ticket:** KR2KNRT9
**Status:** Research / design
**Decision:** **Adopt-modified** — adopt a damped, typed, scoped eigenvector-style
centrality (personalised PageRank), not vanilla global PageRank. Integrate as an
optional additive prior in retrieval scoring, never as the sole rank signal.

> **Implementation status (2026-07).** What shipped is the RRF variant of §6.2
> at the *thought* level: retrieval runs a personalised PageRank (`d = 0.85`,
> 25 iterations, teleport at the query's dense+lexical seeds —
> `src/retrieval/pagerank.rs`) and fuses its top-k as an extra list in the RRF
> seed fusion. The DB-level `AuthorityTable`, TrustRank seeding, and the Sybil
> defences of §5 are not in effect — and none ever were: a `RateClipper`
> (`gossip/sybil.rs`) and `trimmed_mean_merge_hits` (`gossip/merge.rs`) were
> both fully written but never wired to any call site, and were deleted in
> `dc02a18` as verified-unreachable code, changing no runtime behaviour. Treat
> them as unbuilt work with a reference implementation in git history, not as
> defences that were lost. For the current trust model see the
> [Security](https://yesitsfebreeze.github.io/kern/concepts/security) page.
>
> **Measured 2026-07-21: the graph half of that fusion barely changes ranking.**
> Across 8 pairs, creating a reason edge A→B moved neither B's rank nor its score
> — identical to four decimals with the edge and without it. Two causes were
> isolated; one is fixed (a beam threshold that compared the seed and neighbour
> score scales against each other, so a well-matching seed pruned the whole walk),
> and after it 1 of the 8 pairs moves, by one rank. The other 7 are the open cause:
> a neighbour that is already a content hit keeps its seed score, because expansion
> takes the max per entity rather than combining the two pieces of evidence. Recorded
> as a strict xfail (`tests/e2e/test_invariants.py`) and as item 86 in `ROADMAP.md`. The
> edge is created and walkable; it still does not meaningfully reach ranking. One reason it cannot reach
> the *score* is visible in the seed path: RRF settles which entities seed, then
> every survivor is rescored by plain query cosine before re-entering the pipeline
> (`fuse_hybrid_seeds`, `src/retrieval/query.rs`; formerly answer.rs, deleted with the answer leg), so a PageRank list can only change
> membership. Whether it changes membership under a new edge is not established.
> Read the shipped personalised PageRank as wired and running, not as a retrieval
> signal with a demonstrated effect — and read §6 below as an unvalidated design.
> Code paths below reference the pre-1.0 `crates/` layout.

---

## 1. Problem

Federated kern deployments have many peer DBs that emit `Sphere`, `Question`,
`Pulse`, and `Fetch` messages (`crates/gossip/src/types.rs`). Some peers produce
consistently high-signal thoughts that other peers repeatedly reference (via
`etch`-like remote reason edges; today the closest analogue is a remote
`Reason` whose `to_net_id` points at another network — see
`handle_answer` / `resolve_question_from_peer` in
`crates/gossip/src/handler.rs`). Others are noisy or outright adversarial.

Current retrieval (`crates/retrieval/src/score.rs`) ranks by:

- vector/lexical fused score (RRF in `fuse.rs`),
- confidence × base score,
- QBST boost (access recency + count),
- fact bonus,
- `min_deliver_score` floor.

There is **no structural authority signal.** A thought from a peer cited by
thousands of other peers is treated the same as one from a brand-new peer that
has never been referenced.

The LLM "attention sinks" result (a handful of tokens absorb disproportionate
attention mass regardless of query) suggests a graph-theoretic analogue:
authoritative DBs should attract routing weight structurally, not just by
surface similarity.

## 2. Candidates

| Method                 | What it measures                                   | Cost        | Fit                                    |
|------------------------|----------------------------------------------------|-------------|----------------------------------------|
| Degree centrality      | Raw inbound citation count                         | O(1) update | Trivially Sybil-gameable, weak signal  |
| Eigenvector centrality | Fixed point of adjacency × score                   | O(iter · E) | Undamped; diverges on dangling nodes   |
| **PageRank**           | Damped random walk stationary distribution         | O(iter · E) | Handles dangling + cycles + damping    |
| HITS (hub/authority)   | Two-score fixed point                              | O(iter · E) | Nice for bipartite query/doc, less so for general DB graph |
| SALSA                  | Stochastic HITS                                    | O(iter · E) | Overkill                               |
| TrustRank              | Personalised PageRank from seed set of known-good  | O(iter · E) | Strong Sybil defence                   |

**Selection:** PageRank as the base mechanism, with a TrustRank-style
personalisation vector for Sybil resistance, and two scopes:

1. **DB-level authority** — one scalar per peer kern. Nodes = peer DBs
   (`kerns` map, including `remote-*` phantoms from `inject_remote_scope`).
   Edges = remote references (cross-DB `etch`/answer reasons).
2. **Thought-level authority** (optional, phase 2) — nodes = thoughts,
   edges = local reasons. Gives internal hub-detection; useful for split
   heuristics.

## 3. Formula

Classical PageRank on column-stochastic `M` with damping `d`:

```
r = d · M · r + (1 − d) · v
```

- `r ∈ ℝⁿ` — authority vector, `Σ r_i = 1`
- `M_ij` — edge from `j` to `i`, column-normalised
- `d ∈ (0, 1)` — damping; **we use d = 0.85** (standard; empirically robust)
- `v` — teleport distribution; **TrustRank-style**, non-uniform (see §5)

### Damping rationale

- `d = 1` — pure eigenvector; diverges on dangling nodes, no recovery from
  adversarial sinks, no convergence guarantee on disconnected components.
- `d → 0` — rank collapses to the teleport prior, graph is ignored.
- `d = 0.85` — Brin–Page's original value; equivalent expected walk length
  of `1 / (1 − d) = 6.67` hops, matching the "6 degrees" empirical diameter
  of most federated graphs and damping out long chains of collusion.
  Lower `d` (e.g. `0.7`) would be reasonable if we observe high Sybil density
  in practice; tune via `env::RetrievalConfig`.

### Edge weights

Weight each cross-DB edge by:

- `confidence` of the referencing reason (already tracked),
- `1 / fanout_of_source` — prevents one DB from buying influence by
  spamming 10,000 edges out; equivalent to column normalisation.
- optional recency decay `exp(-age / half_life)` mirroring `qbst`.

## 4. Incremental computation on a live graph

The kern graph changes continuously via gossip. Recomputing a full power
iteration on every edge addition is O(iter · E) and unacceptable.

### Strategy: push-based approximate PageRank (Andersen–Chung–Lang)

ACL push maintains a residual vector `residual[v]` and an estimate `r[v]`.
When `residual[v]` exceeds a threshold `ε`, it pushes mass to neighbours and
zeros out. Adding / removing an edge only injects residual locally.

- **Cost per edge update:** `O(1 / ε)` expected pushes, constant in graph
  size for bounded local change.
- **Cost per query:** `O(1)` — just read `r[v]`.
- **Error bound:** `L1` error of `ε · deg(v)` per node; for `ε = 1e-4` and
  ~100 peers, error is bounded and below the noise floor of retrieval scores.

### Tick integration

Add a `tick::TaskKind::PageRankSweep` task, enqueued:

- periodically (e.g. every `pagerank_sweep_interval_secs`, default 300),
- on batched residual overflow (when total `|residual|_1 > ε_batch`).

The sweep runs ACL push until residual converges under `ε`. This amortises
cost and avoids hot-path stalls in gossip handlers.

### Cold start / full recompute

On node boot, run standard power iteration (20–30 sweeps suffice for
`d = 0.85` to converge to `1e-6` L1 error on graphs < 10⁶ edges). Persist
`r` and `residual` alongside the rest of the bincode graph state.

## 5. Sybil resistance

Vanilla PageRank is **provably vulnerable** to Sybil: an attacker running N
fake peers that all cite each other inflates the strongly connected
component's rank as N grows.

### Defences (layered)

1. **TrustRank personalisation.** Set `v` non-uniform: mass concentrated
   on a seed set `S` of known-good peers. The seed set is:
   - peers we have co-resolved questions with above a quality threshold,
   - peers explicitly whitelisted in `env` config,
   - our own node (always).
   Sybil clusters disconnected from `S` receive only `(1 − d)` of rank via
   teleport, proportional to their prior — effectively nil.

2. **Edge-weight caps.** Cap the contribution of any single source DB to
   any target DB at `max_edge_weight` (default `0.1`). Mass laundering
   through a single high-volume edge becomes impossible.

3. **Pulse-coupled validation.** A peer only earns edge weight if its
   `Pulse` has been seen by ≥ k distinct peers in the routing ledger
   (`gossip::ledger`). Single-origin peers cannot bootstrap authority.

4. **Temporal slashing.** Peers whose emitted thoughts are frequently
   superseded (`ThoughtKind::Superseded`) or whose reasons fail resolution
   get their outbound edge weight decayed. Low cost: we already mark
   supersession; just track per-peer supersession rate.

5. **No self-loops.** A peer cannot cite itself for rank purposes.
   Enforce in the edge-ingestion filter.

6. **Rank cap on teleport.** `(1 − d) · v_i` bounded below a ceiling
   prevents the seed set itself from dominating during early-life graphs
   with few edges.

This combination reduces Sybil gain from `O(N)` (vanilla PR) to
`O(edges_into_trust_set)`, which is bounded by real-peer observation.

## 6. Integration sketch

### 6.1 `crates/gossip`

New file `crates/gossip/src/authority.rs`:

```rust
pub struct AuthorityTable {
    pub rank: HashMap<String, f64>,      // kern_id -> r
    pub residual: HashMap<String, f64>,  // ACL residual
    pub fanout: HashMap<String, u32>,    // outbound edge count
    pub trust_seed: HashSet<String>,     // personalisation vector support
}

impl AuthorityTable {
    pub fn observe_edge(&mut self, from: &str, to: &str, weight: f64);
    pub fn push_sweep(&mut self, epsilon: f64);  // ACL iteration
    pub fn full_recompute(&mut self, damping: f64, iters: usize);
    pub fn authority(&self, kern_id: &str) -> f64;
}
```

Hook points:

- `handler::inject_remote_scope` — observe incoming peer; add to graph
  node set (no edge yet).
- `handler::resolve_question_from_peer` — cross-DB reason creation
  (`to_net_id` non-empty) → `observe_edge(origin_net, own_net, conf)`.
- `handler::handle_sphere` — `network_id` mismatch means we learned of
  a foreign scope via this peer; weak edge contribution.

Persistence: serialise `AuthorityTable` in the same bincode store as
graph state; versioned so old snapshots load without rank.

### 6.2 `crates/retrieval`

Extend `RetrievalConfig`:

```rust
pub authority_weight: f64,          // default 0.0 (opt-in)
pub authority_floor: f64,           // ignore rank below this
```

Extend `apply_boosts` in `crates/retrieval/src/score.rs` to add, after the
existing fact_bonus line:

```rust
let authority = authority_table
    .authority(&r.thought.producer_id)
    .max(cfg.authority_floor);
r.score += cfg.authority_weight * authority.ln_1p();
```

`ln_1p` compresses the dynamic range so a single ultra-hub peer cannot
pin every result; additive keeps vector/lexical signals primary.

Alternative: incorporate authority into **RRF** as a third ranked list
(`fuse.rs` already takes `&[&[ThoughtHit]]`). Produces a pure-structural
list that fuses with vector and lexical by rank, not by score magnitude —
arguably cleaner, and sidesteps score-calibration headaches.

### 6.3 Split heuristics

Thought-level PageRank (phase 2) feeds `base::split`:

- High-rank thoughts become **gravitons** — do not split them out of a kern;
  they are the centre of gravity.
- Low-rank thoughts with high local reason count are candidates for
  extraction into a sub-kern (they are "bridge" nodes inflating the parent).

## 7. Failure modes & mitigations

| Failure                               | Mitigation                                                                 |
|---------------------------------------|----------------------------------------------------------------------------|
| Cold-start (empty graph)              | `authority_weight = 0` until `|trust_seed| ≥ 3` and total edges ≥ 50       |
| Oscillation on rapid edge churn       | ACL push is monotone in residual; no oscillation possible                  |
| Partitioned federation                | Each partition's rank is self-consistent; teleport prevents divergence     |
| Rank drift under memory pressure      | Periodic full recompute reconciles push residuals                          |
| Score calibration mismatch            | Use RRF fusion variant (rank-based, not score-based)                       |
| Homogeneous trust seed (monoculture)  | Require `trust_seed` diversity; admin CLI `kern authority seed <id>`       |

## 8. What we explicitly do **not** adopt

- **Pure HITS.** The hub/authority split duplicates information PageRank
  already captures and adds a second eigenvector to maintain.
- **Global synchronised PageRank.** No cross-peer PR consensus protocol;
  each node computes its own view. This matches kern's epistemic stance —
  every DB has its own perspective — and sidesteps Byzantine agreement.
- **Weighted by thought count.** A peer that emits 10⁶ low-quality thoughts
  should not accrue authority; weighting is by *referenced* edges only.

## 9. Decision

**Adopt-modified.**

- Base: PageRank with `d = 0.85`, column-normalised edge weights.
- Modifications: TrustRank personalisation, edge caps, pulse-validated
  edges, temporal slashing, no self-loops, ACL push for incremental
  update, local-view-only (no federated consensus).
- Integration: opt-in scalar (`authority_weight = 0.0` by default) added
  to `score::apply_boosts`, **or** as a third RRF list. RRF calibrates more
  cleanly; the scalar form is the cheaper of the two, one line at the hook.
- Separable second piece: thought-level rank feeding split heuristics — it
  depends on nothing in the first and is tracked in `ROADMAP.md`
  ("Clustering is vector-only"). The authority signal itself is tracked as
  "Peer authority is unbuilt, so Sybil defence has nothing to weight".

## 10. Acceptance checklist

- [x] Applicability: federated authority signal for retrieval & split.
- [x] Formula: standard PR with typed, weighted edges; `d = 0.85`.
- [x] Incremental update: ACL push, tick-scheduled sweeps.
- [x] Sybil considerations: TrustRank seed, edge caps, pulse gate,
      supersession slashing, no self-loops.
- [x] Integration sketch: `gossip::authority` module + `retrieval::score`
      additive boost or RRF list.
- [x] Decision: adopt-modified with reasons.

## References

- Brin, Page — *The Anatomy of a Large-Scale Hypertextual Web Search Engine* (1998).
- Gyöngyi, Garcia-Molina, Pedersen — *Combating Web Spam with TrustRank* (2004).
- Andersen, Chung, Lang — *Local Graph Partitioning using PageRank Vectors* (2006).
- Xiao, Tian et al. — *Efficient Streaming Subgraph Isomorphism* (comparable incremental-update framing).
- Xiao et al. — *Efficient Streaming Algorithms for Graphlet Counting* (for
  incremental graph stat precedent).
- Source: `crates/gossip/src/handler.rs`, `crates/retrieval/src/score.rs`,
  `crates/retrieval/src/fuse.rs`, `crates/gossip/src/types.rs`.
