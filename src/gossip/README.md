# gossip — signed, federated graph exchange over a peer ring

Layer: L5 · Status: **split**
May import: `base`, `graph`, `math`, `tick`, `config`, `util`
Absorbs: `src/gossip_contract.rs`, `src/gossip_handler.rs`, `src/gossip_identity.rs`,
`src/gossip_ledger.rs`, `src/gossip_node.rs`, `src/gossip_privacy.rs`,
`src/gossip_rate.rs`, `src/gossip_ring.rs`, `src/gossip_seen.rs`, `src/gossip_subs.rs`,
`src/gossip_transport.rs`, `src/gossip_types.rs`, `src/identity.rs`

## What it owns

Identity (ed25519), the peer ring, the seen-set, per-origin rate limits, the
ledger, contracts and grants, the chacha20-poly1305 privacy layer, subscription
fan-out, the UDP/TCP transport, and the handler that dispatches a verified
envelope. Federates a `tick::pulse` and applies a `graph::merge` of remote
entities.

## What it must never know

The full tick loop, the model trainer, or the ingest pipeline. It federates
the graph; `loop` decides when.

## Tests

```
cargo test -p gossip
```
