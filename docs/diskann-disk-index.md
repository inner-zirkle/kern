# DiskANN-style disk-resident index — design

**Status (updated 2026-07-21): WIRED (opt-in, entity index only).** DiskANN now
serves the live entity vector search above a configurable threshold — it is the
architecture's designated answer to the unbounded resident-set ceiling. The spill
decision is one line (`src/base/graph.rs:295`), and nothing else bounds the
resident set: `KERN_CAP_DISABLED` (`src/base/constants.rs:29-30`) caps kerns, not
entities, so below the threshold a kern's in-RAM HNSW grows with the corpus.

How it works:
- `GraphGnn`'s entity/gnn/reason indices are a `VectorBackend` enum
  (`src/base/vector_backend.rs`): `Resident(HnswIndex)` or `Disk { snapshot:
  DiskIndex, delta: HnswIndex, tombstones }`.
- `rebuild_index` spills `entity_idx` to a `<data_dir>/diskann/entity` DiskANN
  snapshot once the resident searchable-entity count exceeds `[graph]
  disk_threshold` (default `KERN_CAP_DISABLED` = **never spill**, so small
  deployments are byte-for-byte unchanged). A build/open failure falls back to the
  in-RAM index — a disk error never breaks the graph.
- Post-snapshot writes buffer in the in-RAM `delta` (with tombstones shadowing
  stale/removed snapshot ids). A tick-driven `DiskConsolidate` task folds the
  delta back into a fresh snapshot once it grows past
  `DISK_CONSOLIDATE_MIN_DELTA`, at most hourly, so the delta stays bounded.

Still standalone (`src/base/diskann.rs:157` `build_and_save` + mmap
`DiskIndex::open`/`search` at `src/base/diskann.rs:310` and
`src/base/diskann.rs:385`). This note previously published a "recall@10 ≥ 0.90
vs brute force" figure here; it came from tooling that no longer exists and is
**withdrawn**, not superseded. The retrieval instrument that landed since
(`tests/e2e/test_recall.py` — recall@1 / recall@5 / MRR, no LLM in the scoring loop)
does not replace it and cannot: it scores the default resident backend (spill is
off until `[graph] disk_threshold` is set, and nothing in `tests/e2e/` sets it), its
embeddings are feature-hashed bag of words rather than a real embedding model,
and its floors make it a regression detector — it can say kern got worse, never
that kern or this index is good, and no number it prints is comparable to
anything published elsewhere. No figure for this index against brute force
exists.

**Not shipped:** `gnn_entity_idx`/`reason_idx` stay resident (entity-only
spill), and `DiskIndex` mmaps full `f32` vectors — no product quantization. PQ
is not a pending next step: it is an explicit **non-goal** in `ROADMAP.md`,
re-promotable only if a replacement retrieval metric shows a gap it would close.
The RAM-of-codes decomposition below is retained as reference for that case. The
resident-index gap was tracked in `ROADMAP.md` — "A spilled kern still carries two
resident indexes" — and was **closed 2026-07-21 by measurement rather than by
code**: spilling those two as well costs 122 MB more at 50k entities, and
spilling the entity index alone does not lower resident size at all once queries
touch the snapshot (510.3 MB resident vs 512.1 MB spilled). See item 29 for the
table and `tests/spill_memory.rs` for the instrument.

A figure for this index against brute force now exists, from tooling that does:
`tests/spill_transparency.rs` reports recall@10 = 0.9940 for the spilled path
against exact cosine, versus 1.0000 for the resident HNSW on the same corpus.
The caveats on `tests/e2e/test_recall.py` below apply to it unchanged — feature-hashed
embeddings, a fixed synthetic corpus, a regression detector and not a quality
claim — so it is not comparable to anything published elsewhere.

> **Reality drift since this doc was written.** The original slice-A target below
> (replace `cold.rs`'s O(n) JSONL scan) is OBSOLETE: `cold.rs` and `persist.rs`
> were replaced by an LMDB store (`src/base/store.rs`) with int8-on-disk vectors,
> and `Store::cold_search` is now a BOUNDED scan (capped by `COLD_MAX_ENTRIES`),
> so the cold tier no longer degrades linearly. What DiskANN fixes today is the
> **hot/resident** ceiling: per loaded kern the in-memory `HnswIndex` holds every
> entity vector on the heap and is rebuilt on load, so RSS and load-time grow with
> the kern without bound. PQ (vectors compressed in RAM) is still unbuilt; the
> current `DiskIndex` mmaps full f32 vectors, which moves them off the *heap* but
> not out of RSS — the pages fault straight back in under query load, and the
> corpus is resident a second time in `Kern::entities` regardless (measured; see
> above) — PQ is a separable RAM-of-codes optimization, not a prerequisite, and a
> non-goal today (see above).
> The "ceiling today" list below is retained for historical context.

## The ceiling today

Three things keep the whole corpus in memory and bound it to a single host's RAM:

1. **`HnswIndex` is in-memory** (`src/base/hnsw.rs`). Nodes, the layered graph,
   and quantized vectors all live on the heap; rebuilt from the graph on load.
2. **The graph is a full-RAM bincode blob** (`src/base/persist.rs`). `load_dir`
   decodes an entire kern (`Entity { vector: Vec<f64>, gnn_vector: Vec<f64>, … }`)
   into memory; `save_all` re-encodes it. Load time and RSS scale with corpus.
3. **The cold tier is an O(n) linear scan** (`src/base/store.rs`, absorbed from
   the since-deleted `src/base/cold.rs`): `cold_search` decodes and scores every
   row.

Quantization exists but is **scalar int8 only** (`src/quant.rs`:
`QuantizationMode::{None, Int8}`) — no product quantization yet.

So: a kern with millions of thoughts won't load, won't fit, and cold recall
degrades linearly. That is the corpus-size wall.

## Approach: Vamana + PQ-in-RAM + full-vectors-on-disk

Standard DiskANN decomposition, mapped onto kern's existing pieces:

- **Vamana graph** — a single-layer, long-range-pruned proximity graph (the
  "α-pruning" RobustPrune). kern's HNSW beam search (`beam_search`,
  `prune_neighbors`, the Min/Max heaps) is ~80% of what a Vamana searcher needs;
  the deltas are single-layer (drop `random_level`/layer loop) and disk-resident
  adjacency.
- **PQ-compressed vectors in RAM** — product-quantized codes (e.g. 32–64 bytes
  per vector) kept resident for the approximate distance during graph traversal.
  This is the new quantization mode: extend `QuantizationMode` with `Pq { m, nbits }`
  and a trained codebook (k-means per subspace). `quantized_cosine_distance`
  already abstracts the distance call site.
- **Full vectors on disk, memory-mapped** — exact `f64`/`f32` vectors in a flat,
  fixed-stride file (`vectors.bin`), read on demand via `memmap2` to rerank the
  beam's survivors. Adjacency lives in a parallel `graph.bin` (fixed out-degree
  R, so node i's neighbors are at `i*R`). Search = traverse on PQ codes, fetch a
  bounded number of full vectors for final rerank.

Net: RAM holds PQ codes + the mmap'd page cache, not full vectors. RSS drops from
`O(N·dim·8)` to `O(N·pq_bytes)`.

## How the design decomposes (analysis, not a schedule)

The analysis separated into three independent slices, ordered by how much of the
hot path each one risks. Scheduling is `ROADMAP.md`'s; recorded here is only what
each slice is and what it buys.

**Slice A — disk ANN over the cold tier.** A Vamana index built over the cold
store, replacing a linear scan. Self-contained: the cold tier is append-only,
separately stored, and only the fallback path in `query`. It takes cold recall
from O(n) to O(log n) with no change to the hot path, and exercises the whole
Vamana + mmap + PQ stack on the least-critical tier. *Superseded in part by
reality:* `cold.rs` became the LMDB store and `cold_search` is now a bounded
scan, so what remains here is the index, not the linear-scan fix — the cost that
survives is recorded in `ROADMAP.md` — "The GC sweep is superlinear in three
separate places".

**Slice B — disk-backed hot index for large kerns.** A per-kern threshold:
below it, the in-RAM HNSW; above it, the kern's vectors+adjacency spill to disk
and `search` runs the disk path. Graph metadata (ids, edges, heat, confidence)
stays in RAM far longer than the vectors — vectors are the bulk. This is the
slice that shipped, as the opt-in `VectorBackend::Disk` spill of `entity_idx`
behind `[graph] disk_threshold` — entity index only.

**Slice C — streaming inserts + deletes.** DiskANN is batch-built by default and
kern ingests continuously, so the design takes FreshDiskANN semantics: an in-RAM
delta index for recent inserts, periodic merge into the on-disk Vamana,
tombstones for `forget`/GC, consolidation on the tick beside stigmergy GC. Also
shipped, as the `delta`/`tombstones` fields and the `DiskConsolidate` task.

## Open questions / risks

- **PQ codebook training & drift.** Codebooks need training data and go stale as
  the embedding distribution shifts. When/where to (re)train — on a tick? On
  model swap (which already forces a clean re-embed)? A bad codebook silently
  degrades recall.
- **mmap on Windows.** `memmap2` works cross-platform but file-locking and
  flush semantics differ; the daemon is per-cwd and single-writer, which helps.
- **Incremental Vamana quality.** Naive incremental inserts degrade the graph;
  RobustPrune + periodic full rebuild is the usual answer. Nothing measures that
  degradation: `tests/e2e/test_recall.py` scores the resident path against regression
  floors and never builds a snapshot.
- **Crash consistency.** Disk graph + vectors + the bincode metadata must not
  diverge on a mid-write crash. Write-ahead or atomic rename per segment.
- **Compatibility.** Per repo policy ("no compat, clean base"), the disk format
  is introduced as the only format for large kerns; small kerns keep the
  in-RAM path. No on-disk migration shim.

## Reusable building blocks already in tree

- `src/base/hnsw.rs` — beam search, neighbor pruning, heaps (adapt to 1 layer).
- `src/quant.rs` — quantization seam + int8; extend with PQ.
- `src/base/store.rs` — the LMDB store + cold tier (the original slice-A
  target, `src/base/cold.rs`, was absorbed here).
- `src/base/vector_backend.rs` — the `Resident`/`Disk` backend seam the wiring
  landed on.
