//! Namespace shim for the retrieval pipeline: re-exports every `retrieval_*`
//! stage under `crate::retrieval::<stage>` plus the shared closure aliases.

pub use crate::types::{EmbedFunc, LlmFunc};

pub use crate::retrieval_diversify as diversify;
pub use crate::retrieval_expand as expand;
pub use crate::retrieval_fuse as fuse;
pub use crate::retrieval_gravity as gravity;
pub use crate::retrieval_merge as merge;
pub use crate::retrieval_pagerank as pagerank;
pub use crate::retrieval_query as query;
pub use crate::retrieval_score as score;
pub use crate::retrieval_seed as seed;
