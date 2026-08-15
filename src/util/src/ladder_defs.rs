//! Well-known degradation ladders for kern subsystems.

use crate::ladder::FallbackLadder;

pub static EMBED_LADDER: FallbackLadder = FallbackLadder::new(
    "embed",
    &["remote API", "local GGUF", "none (lexical only)"],
);

pub static LLM_LADDER: FallbackLadder = FallbackLadder::new(
    "llm",
    &["hosted API", "local model", "keyword (no LLM)"],
);

pub static GNN_LADDER: FallbackLadder = FallbackLadder::new(
    "gnn",
    &["full GNN", "PageRank", "uniform weights"],
);

pub static DISTILL_LADDER: FallbackLadder = FallbackLadder::new(
    "distill",
    &["LLM distill", "keyword extract", "no classification"],
);
