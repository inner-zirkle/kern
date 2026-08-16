# transport — the local-RPC substrate

Layer: L4 · Status: **active**
May import: nothing in kern (only `transport-macros` + external crates)
Owns: `src/transport/src/{lib,typed,kern_rpc,hub_rpc}.rs`, `src/transport/macros/`

## What it owns

Typed request/response channels over a Unix domain socket or Windows named
pipe (`typed.rs`), the `service!` codegen, and the kern/hub RPC DTOs +
their `service!`-generated client/server pairs (`kern_rpc.rs`, `hub_rpc.rs`).
No MCP, no HTTP, no other transports — those were deleted 2026-08-16 along
with the MCP surface and, in a second pass, the `wire.rs` module they ran on
(zero remaining callers once MCP was gone). Copied byte-for-byte into every
project that speaks this RPC; it depends on no kern crate.

## What it must never know

What an entity is, what a graph is, that anything is being retrieved. It
carries frames; the surface above gives them meaning.

## ABI

```rust
pub mod typed;     // Adapter, Channel, Codec, Endpoint, bind_kern_listener
pub mod kern_rpc;  // KernRpc (service!-generated)
pub mod hub_rpc;   // HubRpc (service!-generated)
pub use transport_macros::service;
```

`service!` emits `::transport::…` paths (absolute), and the crate declares
`extern crate self as transport;` so those paths resolve from within the crate
itself as well as from dependents.

## Tests

```
cargo test -p transport
```
