use super::dto::{HubStatusRes, ResolveReq, ResolveRes, StopRes, UnloadReq, UnloadRes};

crate::service! {
		pub trait HubRpc {
				async fn resolve(req: ResolveReq) -> ResolveRes;
				async fn status() -> HubStatusRes;
				async fn unload(req: UnloadReq) -> UnloadRes;
				async fn stop() -> StopRes;
		}
}
