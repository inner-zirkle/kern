//! The hub RPC: DTOs, the `service!`-generated client/server pair, and the
//! retrying client CLI and daemons reach the machine hub with.

use std::time::Duration;

use crate::typed::{connect_kern, AdapterError, Channel, Endpoint, JsonEnvelopeCodec};

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

// One registered kern, live or cold. Stats are the hub's last harvest — a
// cold root reports what its daemon last said, not a live count.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KnownRoot {
	pub root: String,
	#[serde(default)]
	pub loaded: bool,
	#[serde(default)]
	pub entities: u64,
	#[serde(default)]
	pub kerns: u64,
	#[serde(default)]
	pub data_bytes: u64,
	#[serde(default)]
	pub last_seen_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HubStatusRes {
	pub ok: bool,
	#[serde(default)]
	pub nodes: Vec<NodeLite>,
	// Every root the hub's persistent registry knows, importance-sorted
	// (entities, then bytes). Empty from hubs predating the registry.
	#[serde(default)]
	pub known: Vec<KnownRoot>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchReq {
	pub text: String,
	// 0 = the hub's default.
	#[serde(default)]
	pub k: u64,
	// Only ask daemons that are already running; never wake a cold kern.
	#[serde(default)]
	pub live_only: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchHit {
	pub root: String,
	// The node's own `query` entity envelope (id, text, score, kind, ...).
	pub entity: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RootErr {
	pub root: String,
	pub err: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchRes {
	pub ok: bool,
	// Merged across every asked kern, score-descending, capped at `k`.
	#[serde(default)]
	pub hits: Vec<SearchHit>,
	// Roots that were asked and failed, or skipped by `live_only`. A partial
	// answer stays `ok` — the misses are named instead of hidden.
	#[serde(default)]
	pub skipped: Vec<RootErr>,
	#[serde(default)]
	pub err: String,
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

crate::service! {
		pub trait HubRpc {
				async fn resolve(req: ResolveReq) -> ResolveRes;
				async fn status() -> HubStatusRes;
				async fn search(req: SearchReq) -> SearchRes;
				async fn unload(req: UnloadReq) -> UnloadRes;
				async fn stop() -> StopRes;
		}
}
