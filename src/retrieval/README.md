# retrieval — hybrid vector + lexical + graph-walk search

Layer: L4 · Status: **split**
May import: `base`, `graph`, `math`, `util`, `llm`, `config`
Absorbs: `src/retrieval.rs`, `src/retrieval_diversify.rs`, `src/retrieval_expand.rs`,
`src/retrieval_pagerank.rs`, `src/retrieval_query.rs`, `src/retrieval_score.rs`,
`src/retrieval_seed.rs`

## What it owns

The retrieval pipeline: seed (lexical + important + dense), expand along reason
edges, fuse the lists (RRF), apply gravity, diversify, and score with the
GNN-refined weights. `retrieval.rs` is the seam that re-exports the stages as
`diversify`/`expand`/`pagerank`/`query`/`score`/`seed`.

## What it must never know

How the weights were trained, how the graph federates, or that a tick loop
schedules it. It retrieves; the loop above decides when.

## Tests

```
cargo test -p retrieval
```
