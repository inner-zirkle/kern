//! The hub RPC: DTOs, the `service!`-generated client/server pair, and the
//! retrying client CLI and daemons reach the machine hub with.

use std::time::Duration;

use crate::transport::typed::{connect_kern, AdapterError, Channel, Endpoint, JsonEnvelopeCodec};

pub const RETRIES: u32 = 2;
pub const RETRY_DELAY_MS: u64 = 100;

impl HubRpcClient<JsonEnvelopeCodec> {
	pub async fn connect_hub() -> Result<Self, AdapterError> {
		let endpoint = Endpoint::hub();
		let mut last_err: Option<AdapterError> = None;
		for i in 0..RETRIES {
			match connect_kern(&endpoint).await {
				Ok(adapter) => {
					let channel = Channel::new(adapter, JsonEnvelopeCodec::new());
					return Ok(HubRpcClient::new(channel));
				}
				// `Endpoint::hub()` is `scoped()` too, so the hub socket carries the
				// same squattable name as a node's — and the same verdict: an endpoint
				// this user does not own will not become theirs on the second try.
				Err(e @ AdapterError::UntrustedEndpoint(_)) => return Err(e),
				Err(e) => {
					last_err = Some(e);
					if i + 1 < RETRIES {
						tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
					}
				}
			}
		}
		Err(last_err.unwrap_or_else(|| AdapterError::Other("no hub endpoint".into())))
	}
}

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResolveReq {
	pub root: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResolveRes {
	pub ok: bool,
	#[serde(default)]
	pub endpoint: String,
	#[serde(default)]
	pub spawned: bool,
	#[serde(default)]
	pub err: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct NodeLite {
	pub root: String,
	pub endpoint: String,
	pub pid: u32,
	pub alive: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HubStatusRes {
	pub ok: bool,
	#[serde(default)]
	pub nodes: Vec<NodeLite>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StopRes {
	pub ok: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UnloadReq {
	pub root: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UnloadRes {
	pub ok: bool,
	#[serde(default)]
	pub existed: bool,
	#[serde(default)]
	pub err: String,
}

crate::transport::service! {
		pub trait HubRpc {
				async fn resolve(req: ResolveReq) -> ResolveRes;
				async fn status() -> HubStatusRes;
				async fn unload(req: UnloadReq) -> UnloadRes;
				async fn stop() -> StopRes;
		}
}
