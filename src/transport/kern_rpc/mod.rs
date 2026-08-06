pub mod auth;
pub mod client_local;
pub mod dto;
pub mod svc;

pub use auth::{present_auth, verify_auth, AuthReq};
pub use dto::{CallToolReq, CallToolRes, HealthRes, ListToolsReq, ListToolsRes, ShutdownRes};
pub use svc::{serve_kern_rpc, KernRpc, KernRpcClient};
