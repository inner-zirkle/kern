//! Namespace shim for the hub RPC: re-exports client/dto/svc so
//! `crate::transport::hub_rpc::X` resolves unchanged.

pub use crate::transport_hub_rpc_client as client;
pub use crate::transport_hub_rpc_dto as dto;
pub use crate::transport_hub_rpc_svc as svc;

pub use crate::transport_hub_rpc_dto::{
	HubStatusRes, NodeLite, ResolveReq, ResolveRes, StopRes, UnloadReq, UnloadRes,
};
pub use crate::transport_hub_rpc_svc::{serve_hub_rpc, HubRpc, HubRpcClient};
