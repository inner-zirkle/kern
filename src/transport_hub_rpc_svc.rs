//! The hub RPC service definition (`service!`-generated).

use crate::transport_hub_rpc_dto::{
	HubStatusRes, ResolveReq, ResolveRes, StopRes, UnloadReq, UnloadRes,
};

crate::transport::service! {
		pub trait HubRpc {
				async fn resolve(req: ResolveReq) -> ResolveRes;
				async fn status() -> HubStatusRes;
				async fn unload(req: UnloadReq) -> UnloadRes;
				async fn stop() -> StopRes;
		}
}
