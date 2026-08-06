use crate::transport_kern_rpc_dto::{CallToolReq, CallToolRes, HealthRes, ListToolsReq, ListToolsRes, ShutdownRes};

crate::transport::service! {
		pub trait KernRpc {
				async fn health() -> HealthRes;
				async fn shutdown() -> ShutdownRes;
				async fn call_tool(req: CallToolReq) -> CallToolRes;
				async fn list_tools(req: ListToolsReq) -> ListToolsRes;
		}
}
