//! retrieval — hybrid vector + lexical + graph-walk search.
//!
//! Seeds (lexical + important + dense), expands along reason edges, fuses the
//! lists (RRF), applies gravity, diversifies, and scores with the GNN-refined
//! weights. Built on `graph` (the entity graph + indexes), `math` (vectors),
//! `base` (vocabulary), `llm` (the embed closures a seed needs), `util`.
//!
//! Layer: L4 · May import: `base`, `graph`, `math`, `util`, `llm`.

pub mod id_detail;
pub mod retrieval;
pub mod retrieval_diversify;
pub mod retrieval_expand;
pub mod retrieval_intent;
pub mod retrieval_pagerank;
pub mod retrieval_query;
pub mod retrieval_score;
pub mod retrieval_seed;

pub use retrieval::*;
