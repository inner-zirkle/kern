//! math — the numeric substrate: vector ops and quantization.
//!
//! Cosine/dot/norm over `&[f32]`, online softmax, and the int8 quantization
//! the store uses to pack embeddings. Stands on `base` for the entity kinds a
//! vector is tagged with; nothing above it stands on anything heavier.
//!
//! Layer: L2 · May import: `base`, `util`.

pub mod math;
pub mod quant;

pub use math::*;
