use std::time::Duration;

use crate::typed::{connect_kern, AdapterError, Channel, Endpoint, JsonEnvelopeCodec};

use super::svc::HubRpcClient;

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
