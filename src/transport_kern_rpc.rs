//! Namespace shim for the kern RPC: re-exports auth/client/dto/svc so
//! `crate::transport::kern_rpc::X` resolves unchanged.

pub use crate::transport_kern_rpc_auth as auth;
pub use crate::transport_kern_rpc_client_local as client_local;
pub use crate::transport_kern_rpc_dto as dto;
pub use crate::transport_kern_rpc_svc as svc;

pub use crate::transport_kern_rpc_auth::{present_auth, verify_auth, AuthReq};
pub use crate::transport_kern_rpc_dto::{
	CallToolReq, CallToolRes, HealthRes, ListToolsReq, ListToolsRes, ShutdownRes,
};
pub use crate::transport_kern_rpc_svc::{serve_kern_rpc, KernRpc, KernRpcClient};
