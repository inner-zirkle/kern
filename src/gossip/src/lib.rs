//! gossip — signed, federated graph exchange over a peer ring.
//!
//! Identity (ed25519), the peer ring, the seen-set, rate limits, the ledger,
//! contracts and grants, the privacy layer (chacha20-poly1305 sealed payloads),
//! subscription fan-out, the UDP/TCP transport, and the handler that dispatches
//! a verified envelope. Built on `tick` (the pulse it federates), `graph`
//! (the entity/reason merge it applies), `config`, `math`, `base`, `util`.
//!
//! Layer: L5 · May import: `base`, `graph`, `math`, `tick`, `config`, `util`.

pub mod gossip_contract;
pub mod gossip_handler;
pub mod gossip_identity;
pub mod gossip_ledger;
pub mod gossip_node;
pub mod gossip_privacy;
pub mod gossip_rate;
pub mod gossip_ring;
pub mod gossip_seen;
pub mod gossip_subs;
pub mod gossip_transport;
pub mod gossip_types;
pub mod identity;
