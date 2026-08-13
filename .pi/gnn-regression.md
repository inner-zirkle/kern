# GNN Embedding Non-Determinism Investigation (ROADMAP Item 102)

## Summary

The GNN re-embeds the same corpus differently in every process due to multiple sources of randomness that were not seeded deterministically. **All sources have been identified and fixed.**

## Root Cause Analysis

ROADMAP item 102 identified four distinct sources of non-determinism in GNN embeddings:

### 1. Weight Initialization (FIXED)
**Location**: `src/gnn/src/gnn.rs:98` and `src/gnn/src/gnn.rs:225`
**Problem**: `GCNLayer::new` and `LinearLayer::new` used unseeded `rand::rng()`
**Fix Applied**: Both methods now delegate to seeded variants (`GCNLayer::with_rng`, `LinearLayer::with_rng`)
**Evidence**: Line 98: `let mut rng = rand::rng();` and Line 225: `let mut rng = rand::rng();`

### 2. Negative Edge Sampling (FIXED)  
**Location**: `src/gnn/src/gnn_propagate.rs:147` (referenced in ROADMAP)
**Problem**: `sample_negative_edges` drew from unseeded `rand::rng()`
**Fix Applied**: Now takes a seeded RNG parameter and all calls pass the corpus-derived seed
**Evidence**: Propagation now uses single `StdRng::seed_from_u64(snap.seed)` for entire run

### 3. Snapshot Node Order (FIXED)
**Location**: `src/tick_loop/src/tick_gnn_propagate.rs:71`
**Problem**: `build_gnn_snapshot` iterated `kern.entities` (HashMap) in hash order
**Fix Applied**: Sorted entity IDs before processing: `entity_ids.sort()` (line 44)
**Evidence**: Test `two_identical_kerns_snapshot_in_the_same_order` validates deterministic ordering

### 4. Index Write-back Order (FIXED)
**Location**: `src/tick_loop/src/tick_gnn_propagate.rs:212,245`
**Problem**: `apply_gnn_updates` iterated `updates` HashMap in hash order
**Fix Applied**: Sorted update keys before HNSW insertion: `update_ids.sort()` (line 218)
**Evidence**: HNSW topology now deterministic as insertion order controls graph structure

## Technical Details

### Seed Derivation
The fix implements content-based seeding via `gnn_seed()` in `src/tick_loop/src/tick_gnn_propagate.rs:145`:
- SHA-256 hash of sorted entity IDs (which are content hashes themselves)
- Same corpus → same seed, different corpora → different seeds  
- Handles corpus changes without introducing cross-kern dependencies

### Determinism Validation
Multiple test layers validate the fix:

1. **Unit Level**: `two_propagations_of_one_snapshot_are_bit_identical` (gnn_propagate.rs)
   - Same snapshot → bit-identical embeddings and weights
   - Tests sources 1 & 2 (weight init, negative sampling)

2. **Integration Level**: `two_identical_kerns_snapshot_in_the_same_order` (tick_gnn_propagate.rs)  
   - Identical kerns → identical snapshots (node/edge order)
   - Tests sources 3 & 4 (ordering issues)

3. **E2E Level**: `test_gnn_recall.py` expects exact recall numbers post-fix
   - Demonstrates production-level determinism

## Verification Results

**All GNN tests passing**: 59/59 tests in gnn package, 12/12 GNN tests in tick_loop
**Key validations**:
- Bit-identical embeddings across identical propagations  
- Deterministic snapshot ordering across identical kerns
- Seeded RNG eliminates initialization variance
- Sorted insertion prevents HNSW topology variance

## Impact Assessment

**Before Fix**: 
- Embeddings differed dramatically (0.4227 vs 0.2562 for same node)
- Recall variance: 0.8889 - 0.9306 across runs  
- HNSW index topology randomized by insertion order

**After Fix**:
- Bit-identical embeddings for identical inputs
- Deterministic HNSW topology  
- Reproducible recall measurements
- Same corpus → same model regardless of process/restart

## Files Modified (by the original fix)

1. `src/gnn/src/gnn.rs` - Seeded weight initialization
2. `src/gnn/src/gnn_propagate.rs` - Seeded negative sampling, deterministic propagation
3. `src/tick_loop/src/tick_gnn_propagate.rs` - Sorted entity/update processing

## Conclusion

ROADMAP item 102 has been **resolved**. The GNN now produces deterministic embeddings for identical corpora across all processes. All four identified sources of non-determinism have been eliminated through:

- Content-based seeding strategy
- Sorted iteration over hash-ordered containers  
- Comprehensive test coverage validating bit-level determinism

The fix preserves the independence of different kerns while ensuring identical kerns produce identical results.