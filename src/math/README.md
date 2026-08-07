# math — the numeric substrate

Layer: L2 · Status: **split**
May import: `base`, `util`
Absorbs: `src/math.rs`, `src/quant.rs`

## What it owns

Vector arithmetic over `&[f32]` (cosine, dot, norm, average, distance), online
softmax for retrieval scoring, and the int8 quantization the store packs
embeddings into. One authoritative copy of the arithmetic three layers above
would otherwise drift on — retrieval, the model, the grader.

## What it must never know

That a vector is an embedding of an entity, that anything is being trained, or
that a distance is a retrieval score. Numbers in, numbers out.

## ABI

```rust
pub fn cosine(a: &[f32], b: &[f32]) -> f32;
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f32;
pub fn average_vec(out: &mut [f32], samples: &[&[f32]]);
pub fn reason_id(kind: base::base_types::ReasonKind, subject: &str) -> String;
pub mod quant;  // QuantizedEmbedding, quantize/dequantize
```

## Invariants

- `cosine` of a zero vector is 0, never NaN.
- Quantization is symmetric int8; round-trip error is bounded and measured.

## Tests

```
cargo test -p math
```
