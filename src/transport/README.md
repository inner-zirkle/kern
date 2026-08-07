# transport — the portable JSON-RPC wire layer

Layer: L4 · Status: **split**
May import: nothing in kern (only `transport-macros` + external crates)
Absorbs: `src/transport/{wire,typed,mcp,http,mod}.rs`, `src/transport/macros/`,
`src/transport_kern_rpc.rs`, `src/transport_hub_rpc.rs`

## What it owns

Seven framings of one JSON-RPC contract (tcp/unix/stdio/http/sse/ws/udp), typed
request/response channels + the `service!` codegen, the MCP envelope, HTTP
server glue, and the kern/hub RPC DTOs + their `service!`-generated pairs.
Copied byte-for-byte into every project that speaks JSON-RPC; it depends on no
kern crate.

## What it must never know

What an entity is, what a graph is, that anything is being retrieved. It
carries frames; the surface above gives them meaning.

## ABI

```rust
pub mod wire;      // select, serve, Dispatch, Transport
pub mod typed;     // Adapter, Channel, Codec, Endpoint, bind_kern_listener
pub mod mcp;       // McpServer, dispatch, serve_stdio, ToolSchema
pub mod http;      // serve_http
pub mod kern_rpc;  // AuthReq, KernRpc (service!-generated)
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
