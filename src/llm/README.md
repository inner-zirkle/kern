# llm — the embedder and reasoner client

Layer: L2 · Status: **split**
May import: `util`
Absorbs: `src/llm.rs`

## What it owns

A blocking façade over an OpenAI-compatible HTTP endpoint: `embed`, `reason`,
`embed_chunks`, retry, timeout, and a `LogThrottle` that collapses a repeating
failure to one warn line. Owns the canonical `DEFAULT_REASON_TIMEOUT_SECS`
that `config` reads down — so `config` never reaches back up to a runtime type.

## What it must never know

What an entity is, that anything is being retrieved, or that a tick loop
exists. It posts JSON and returns vectors or text.

## ABI

```rust
pub const DEFAULT_REASON_TIMEOUT_SECS: u64 = 600;
pub struct Client { /* ... */ }
pub fn is_local_url(url: &str) -> bool;
pub fn is_loopback_url(url: &str) -> bool;
pub fn is_openai_compat(url: &str) -> bool;
pub fn is_wsl() -> bool;
pub const EMBED_NUM_CTX: u32; pub const EMBED_KEEP_ALIVE: &str;
pub const REASON_NUM_CTX: u32; pub const REASON_KEEP_ALIVE: &str;
```

## Tests

```
cargo test -p llm
```
