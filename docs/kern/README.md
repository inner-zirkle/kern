# Research & rationale

The notes in this directory are the design rationale behind kern's
self-organizing behavior — the models, proofs, and trade-off
analyses that the implementation is built on. No source file cites them. The
site's [Decisions](../site/content/docs/decisions/index.mdx) section carries one
page per note and is where a decision's current standing is written; these notes
keep the reasoning that produced it.

These are reference material, not a tutorial. For how to *use* kern, start with
the [README](../../README.md) and the
[Memory Bank guide](../site/content/docs/howto/memory-bank.mdx);
for what it is and why it exists, read the [Vision](../windmill/VISION.md).

Most notes are point-in-time studies: each opens with a status line saying what
has shipped since, and code paths inside them may reference the older `crates/`
workspace layout (everything now lives under `src/`). The bodies are left as
written; where measurement has since contradicted a decision, the status line
says so and the body does not.

## Self-organization

- **[Stigmergy for self-improving memory](../stigmergy-self-improving.md)** — the
  heat / decay / evict / cluster loop that keeps the hot graph small without
  manual gardening. Implemented by `tick::stigmergy`.
- **[PageRank for authority](../pagerank-authority.md)** — how graph centrality
  was meant to weight retrieval. Personalised PageRank runs at seed fusion, but a
  reason edge is measured to move ranking barely at all (1 of 8 pairs, by one
  rank), so the weighting is close to unobserved.
- **[Bayesian belief](../bayesian-belief.md)** — multi-observer truth convergence:
  how confidence updates as independent observations accumulate.
- **[Wikipedia edit-convergence](../wikipedia-edit-convergence.md)** — the
  NPOV-style model for converging on neutral, durable thoughts.

## Retrieval & indexing

- **[DiskANN-style disk-resident index](../diskann-disk-index.md)** — the design
  for the on-disk ANN index, now wired as the opt-in `VectorBackend::Disk`
  spill for the entity index (off by default; see `base::diskann` and
  `base::vector_backend`).

## Runbooks

- **[Running kern against vLLM](../vllm.md)** — the one page here that is not a
  design study: how a `[reason]` / `[answer]` / `[embed]` leg points at an
  OpenAI-compatible server, and the WSL2 flags each of which clears one vLLM
  startup failure.

## Planning

All open work lives in one file: **[ROADMAP.md](../windmill/ROADMAP.md)**.
This directory holds reference and measurement records; any forward-looking
plans a note carries are historical, and ROADMAP.md alone is authoritative
for new work.
