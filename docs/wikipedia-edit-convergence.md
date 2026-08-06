# Wikipedia Edit-Convergence Model for NPOV Thoughts

Ticket: HCXW4XG5
Status: research / design decision

> **Implementation status (2026-07).** The hybrid decision stands: supersede
> (`EntityStatus::Superseded` + `Supersedes` edges) remains the default, and
> alternate phrasings of an existing thought are captured non-destructively as
> `ReasonKind::Rephrase` edges. The proposed `ReasonKind::Edit` rationale edge
> and the 3RR-lite producer rate-limit were not built; the trigger conditions
> below have not fired.

## Problem

kern thought statements today use a **supersede model**: when a thought is
updated via entity resolution, the old thought is marked
`ThoughtKind::Superseded` with `superseded_by` pointing at the replacement.
`source_index` maps `external_id` to the current thought id, so readers see
only the latest. There is no history, no reason text on *why* the statement
changed, and no mechanism for multi-agent convergence when two producers
disagree about the same external source.

The user's framing: "multi-user convergence = bell curve." Wikipedia is the
existence-proof that a noisy crowd of editors can converge on a single stable
article for contested topics. Can its mechanics inform how kern handles
multi-agent edits on the same `external_id`?

## What Wikipedia actually does

Four coupled mechanisms, none of which is sufficient alone:

1. **Full edit history (MediaWiki revisions).** Every save is an append-only
   revision with author, timestamp, byte-diff, and edit summary. Nothing is
   ever truly overwritten. Revert = copy an older revision forward.
2. **Bounded revert-war: 3RR.** The "three-revert rule" caps any single
   editor at three reverts of the same material within 24 hours. Violation
   triggers an admin block. This is the explicit *bound* on edit wars —
   without it, two determined editors oscillate forever.
3. **Talk pages.** Every article has a parallel discussion surface where
   editors argue for their version with citations. Consensus on talk is
   what actually resolves disputes; the article itself is a downstream
   artifact. NPOV ("neutral point of view") is the policy target.
4. **Reputation and privilege tiers.** Anonymous < registered < autoconfirmed
   < rollbacker < admin < arbcom. Higher-reputation edits stick longer,
   protected pages restrict who can edit, and repeat offenders lose write
   access. Reputation is earned through edit count plus low-revert ratio.

Convergence happens because (1) preserves evidence, (2) rate-limits the loud
minority, (3) moves disagreement to a slower channel where citations beat
assertions, and (4) asymmetrically weights trusted contributors.

## Mapping onto kern

| Wikipedia                  | kern analog (if adopted)                          |
|----------------------------|---------------------------------------------------|
| Revision history           | `Vec<StatementRevision>` per statement index      |
| Edit summary               | `Reason` edge of new kind `Edit` with text        |
| 3RR                        | Rate-limit per `producer_id` per `external_id`    |
| Talk page                  | Existing `Reason`-graph around the thought        |
| Reputation tier            | Per-`producer_id` score from `access_count`+merges|
| NPOV article               | The *current* statement set (what `text()` emits) |
| Page protection            | no per-row scoping in kern (removed 2026-07-22); a lock flag would be its own field |

The talk-page analog is the single strongest fit: kern already has a typed
edge graph (`ReasonKind::Provenance`, `Question`, `Supersedes`). A
`ReasonKind::Edit` whose `text` is the edit rationale gives us "why does
this thought say X" for free — queryable via existing beam search.

## Decision: hybrid, not full versioning

Adopting Wikipedia wholesale is YAGNI for a single-agent or small-agent
deployment. kern is not a public encyclopedia; most writes come from
cooperating agents with shared goals, not adversaries. Full revision
history plus 3RR plus reputation is a large, coupled feature set.

Adopted now (hybrid):

- **Keep supersede as the default.** It is correct for the common case:
  one producer, monotonic improvement. `ThoughtKind::Superseded` plus
  `superseded_by` already forms a chain that can be walked backwards, so
  minimal history is *already* present for free.
- **Add a `ReasonKind::Edit` edge** from the new thought to the superseded
  one, carrying the rewrite rationale as `text`. This is the talk-page
  analog with zero schema churn — reasons already have `text`, `vector`,
  and enrichment. Answers "why does this thought now say X."
- **Soft rate-limit in `ingest`**: if the same `producer_id` supersedes
  the same `external_id` more than N times in a window, mark the producer
  for review rather than block. 3RR-lite.

Deferred (not building now):

- Parallel statement revisions stored inline. The supersede chain *is* the
  history; we do not need a second mechanism until the chain proves
  insufficient (long chains, need diff-by-byte, need blame-per-token).
- Reputation scoring. Wait until we have a multi-agent deployment where
  adversarial or low-quality producers actually appear. Premature.
- Locked/protected thoughts. kern carries no per-row scoping; add only when
  a real threat model exists.

Trigger conditions that would flip the decision toward full versioning:

1. Supersede chains routinely exceed ~5 hops on the same `external_id`.
2. Two or more `producer_id`s ping-pong the same external source.
3. Users request byte-level blame ("who wrote *this word*").

Until one of those fires, the hybrid is KISS-compliant and keeps the
current supersede model intact.

## Acceptance criteria check

- [x] Research note on applicability to thought statements — above.
- [x] Decision: **hybrid** — keep supersede, add `ReasonKind::Edit` with
      rationale text, soft rate-limit in ingest. Full versioning and
      reputation deferred behind explicit trigger conditions.
