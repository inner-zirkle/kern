# Bayesian Belief Networks for Multi-Observer Truth Convergence

Status: design research (ticket 469D9Z75)
Scope: whether thoughts and reasons should carry a `(belief, uncertainty)` tuple
instead of today's scalar `confidence` / `score`, and how that tuple updates as
new observers arrive.

> **Implementation status (2026-07).** Adopted: entities carry Beta-distributed
> confidence (`conf_alpha`/`conf_beta` at `src/base/types.rs:290-291`, with
> `observe_support` / `observe_contradict` at `src/base/types.rs:411` and
> `src/base/types.rs:417`), seeded `(1+conf, 1+(1−conf))` exactly as §3 proposes
> (`src/ingest/place.rs:16-18`). Under federation the tuple is deliberately
> **replica-local** — never merged from a peer (`src/base/merge.rs:93`).
>
> Contradiction detection shipped, but not in either form §2 weighed. There is no
> `ReasonKind::Contradicts` (`src/base/types.rs:77-86` lists the seven kinds that
> exist), no NLI model and no `stance` argument on ingest: the accept path only
> ever supports (`src/base/accept.rs:162`). What moves `β` is GNN propagation —
> each refreshed embedding is compared against the entity's own vector and scores
> `observe_support` at cosine alignment ≥ 0.5, `observe_contradict` below it
> (`src/tick/gnn_propagate.rs:229-233`). So the "observer" that can contradict is
> the graph's own learned view, not a second agent. Since the propagation chain
> became fallible end to end (`src/gnn/model.rs:19`, `src/gnn/model.rs:30`) a
> failed forward or backward applies no updates at all, so belief moves only on a
> propagation that completed.
>
> Observer-reputation weighting was not built. Code paths below reference the
> pre-1.0 `crates/` layout.

## Thesis

In kern's intended use — a shared memory substrate for many agents, possibly
many humans — **convergence across independent observers is the truth signal.**
One agent asserting "X" carries weak evidence; ten agents, drawn from different
contexts, asserting "X" should approach certainty. A single outlier claiming
"not X" in that population should be absorbed rather than dominate. Bayesian
belief updating is the canonical formal framework for this claim: a prior over
the truth of a proposition, updated by weighted likelihoods from each observer.

Today kern records a single scalar `score: f64` on each `Thought`
(see `crates/base/src/types.rs`), populated at ingest time from the `conf` arg
on `mcp__kern__ingest` and carried through `ingest::build_chunk_thought`. That
scalar has no variance, no sample count, no way to distinguish "one observer,
conf=0.9" from "twenty observers, mean=0.9".

## 1. Representation proposal

Replace (or wrap) the scalar `score` with a **Beta-distributed belief**:

```rust
pub struct Belief {
  /// Pseudo-count of supporting observations (successes + prior α).
  pub alpha: f64,
  /// Pseudo-count of contradicting observations (failures + prior β).
  pub beta:  f64,
}
```

A Beta(α, β) distribution is the conjugate prior for a Bernoulli parameter —
i.e. the probability that the proposition is true. It gives two derived
quantities for free:

- **point belief**: `p = α / (α + β)` — the posterior mean, a number in `[0,1]`
- **uncertainty**: `var = αβ / ((α+β)² (α+β+1))` — shrinks as `α+β` grows

With `α = β = 1` (uniform prior) a new thought starts at `p = 0.5`,
`var = 1/12 ≈ 0.083`. After ten concordant observers it sits near `α ≈ 11,
β ≈ 1`, giving `p ≈ 0.92` and `var ≈ 0.006` — tight belief. One dissenter in
that population moves it to `α ≈ 11, β ≈ 2`, `p ≈ 0.85`: visible but not
catastrophic. This is exactly the outvoting property the ticket calls for.

**Serialisation cost**: two `f64` replacing one `f64` → +8 bytes per thought.
Negligible next to the embedding `Vec<f64>` (thousands of floats).

## 2. Update rule for conflicting observations

Each ingest event is one Bernoulli observation, weighted by the observer's
declared confidence `w ∈ [0,1]` (what `conf` supplies today):

```
on support(w):    α += w
on contradict(w): β += w
```

This is the standard conjugate update with fractional counts — it lets a
tentative observer (`w=0.3`) contribute a third of a full vote, while a
confident one (`w=0.95`) contributes nearly a full vote. A `conf=1.0` assertion
by an agent that is later contradicted still only carries one unit of weight;
no single observer can pin belief.

**Contradiction detection** in kern already exists in latent form: when an
ingest's embedding is within `dedup_threshold` cosine similarity of an existing
thought, `update_existing_thought` merges them. Extend this: if the new text is
semantically *opposite* (to be decided — candidates: NLI model, explicit
`supports`/`contradicts` flag on the MCP ingest tool, or a `Reason` edge of a
new `Contradiction` kind), the update hits `β` instead of `α`.

**Observer weighting** (future): multiply `w` by an observer reputation score
derived from their agreement with graph consensus over time. This is the same
mechanism PageRank gives authority in `docs/pagerank-authority.md`; the two
designs compose.

**Decay**: optionally damp `α` and `β` each tick by a factor `γ < 1`. Old
evidence matters less; genuinely stable facts keep getting reinforced and stay
high; stale consensus can be unseated by a burst of new observers. Ties into
existing tick scheduler in `tick/`.

## 3. Mapping onto `mcp__kern__ingest conf`

Current flow (see `crates/ingest/src/lib.rs:548`): `conf` is passed through
`build_chunk_thought` and stored verbatim as `Thought.score`.

Proposed flow:

- first-time ingest of a thought → initialise `belief = Belief { alpha: 1.0 + conf, beta: 1.0 + (1.0 - conf) }`.
  The `+1` per side is the uniform prior; the conf value biases the opening
  posterior toward the observer's stated confidence without committing to it.
- dedup merge (`update_existing_thought`) → `alpha += conf`. The existing
  function already takes `new_score: f64`; extend it to take a
  `&Belief` delta and fold it into the stored tuple.
- new `contradict` path (needs MCP surface, e.g. extra ingest param
  `stance: "support" | "contradict"`, default `"support"`) → `beta += conf`.

For backward compatibility the wire type keeps a computed `score: f64 = α/(α+β)`
field at serialisation time so existing CLI / HTTP consumers don't break. The
raw tuple is additive.

## 4. Cost / benefit

Benefit:
- **Convergence is explicit.** `(α, β)` makes it trivial to answer "how many
  observers agree and how strongly" — currently lost.
- **Uncertainty surfaces in retrieval.** Scoring in `retrieval/` can down-weight
  high-variance thoughts in beam search, or surface them as "contested" in
  answers instead of hiding the disagreement.
- **Resistance to adversarial or mistaken single sources.** A lone high-`conf`
  assertion no longer fixes truth; it just tips a prior.
- **Composable with PageRank authority and GNN learning.** Observer weight is
  the natural input to the α-β update.

Cost:
- +8 bytes per thought in persisted `bincode`. Negligible.
- One extra param (or inferred stance) on ingest. Additive, optional.
- Contradiction detection is the hard part — either a new `ReasonKind`
  (cheap, explicit, agent-driven) or an NLI model (expensive, not in current
  dep allow-list). Recommend starting with explicit: let agents declare
  `stance` on ingest, accept that uncurated text defaults to `"support"`.
- Retrieval code that sorts on `score` needs a migration: either keep the
  derived mean, or use lower confidence bound (`p - k·sqrt(var)`) for ranking.

**Recommendation**: adopt the tuple. The 8-byte cost is trivial; the semantic
gain — "truth is what many independent observers converge on" becoming a
computable, persistable quantity — is exactly the thesis of the project. Start
with support-only updates tied to the existing `conf` param; add explicit
contradiction through a new `ReasonKind::Contradicts` edge in a follow-up.

## 5. Open questions

- Should `Reason` edges carry belief too? Probably yes, symmetrically. An edge
  claiming "A causes B" is itself a proposition that observers can corroborate.
- Decay rate γ — per-tick, per-day, or tied to access frequency?
- Interaction with `Superseded` kind: does superseding reset belief or inherit?
- UI surface in MCP `query` responses: expose `(belief, uncertainty)` directly,
  or only the derived scalar? Leaning toward both (mean as primary, tuple as
  optional field).
