use std::time::Duration;

use crate::typed::{connect_kern, AdapterError, Channel, Endpoint, JsonEnvelopeCodec};

use super::auth::{present_auth, AuthReq};
use super::svc::KernRpcClient;

pub const RETRIES: u32 = 5;
pub const RETRY_DELAY_MS: u64 = 100;

impl KernRpcClient<JsonEnvelopeCodec> {
	pub async fn connect_local(auth: &AuthReq) -> Result<Self, AdapterError> {
		Self::connect_endpoint(&Endpoint::kern(), auth).await
	}

	pub async fn connect_endpoint(endpoint: &Endpoint, auth: &AuthReq) -> Result<Self, AdapterError> {
		Self::connect_endpoint_with_retry(
			endpoint,
			auth,
			RETRIES,
			Duration::from_millis(RETRY_DELAY_MS),
		)
		.await
	}

	/// The handshake is part of connecting: a `KernRpcClient` only ever exists
	/// on a channel the daemon has already admitted.
	pub async fn connect_endpoint_with_retry(
		endpoint: &Endpoint,
		auth: &AuthReq,
		retries: u32,
		base_delay: Duration,
	) -> Result<Self, AdapterError> {
		let mut last_err: Option<AdapterError> = None;
		for _ in 0..retries {
			match connect_kern(endpoint).await {
				Ok(adapter) => {
					let mut channel = Channel::new(adapter, JsonEnvelopeCodec::new());
					// Propagated, never retried: a refusal is the daemon's verdict on
					// this caller, and it will say the same thing five times. Retrying
					// would only delay the report; swallowing it would tell the caller
					// nothing is serving, which is how a CLI ends up writing behind a
					// daemon that is very much there.
					present_auth(&mut channel, auth).await?;
					return Ok(KernRpcClient::new(channel));
				}
				// Also propagated, never retried, and for the same reason from the
				// other side: the endpoint is bound by something this user does not
				// own. Waiting cannot make it ours, and the retry loop exists for a
				// daemon that has not finished starting, not for one that is not
				// there at all.
				Err(e @ AdapterError::UntrustedEndpoint(_)) => return Err(e),
				Err(e) => last_err = Some(e),
			}
			tokio::time::sleep(jittered(base_delay)).await;
		}
		Err(last_err.unwrap_or_else(|| AdapterError::Other("no endpoint".into())))
	}
}

fn jittered(base: Duration) -> Duration {
	let base_ms = base.as_millis() as u64;
	if base_ms == 0 {
		return base;
	}
	let nanos = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.subsec_nanos() as u64)
		.unwrap_or(0);
	let half = base_ms / 2;
	Duration::from_millis(half + (nanos % (half + 1)))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn bogus_endpoint() -> Endpoint {
		#[cfg(unix)]
		{
			Endpoint::Unix(std::path::PathBuf::from(
				"/nonexistent/kern-test-bogus.sock",
			))
		}
		#[cfg(windows)]
		{
			Endpoint::NamedPipe(r"\\.\pipe\kern-test-bogus-nonexistent".to_string())
		}
	}

	#[test]
	fn jittered_stays_within_half_to_full_and_zero_stays_zero() {
		assert_eq!(jittered(Duration::ZERO), Duration::ZERO);
		for _ in 0..64 {
			let d = jittered(Duration::from_millis(100));
			assert!(
				d >= Duration::from_millis(50) && d <= Duration::from_millis(100),
				"jitter must stay in [base/2, base], got {d:?}",
			);
		}
	}

	#[tokio::test]
	async fn connect_endpoint_gives_up_after_exhausting_retries() {
		let res = KernRpcClient::connect_endpoint_with_retry(
			&bogus_endpoint(),
			&AuthReq::new("t"),
			3,
			Duration::from_millis(1),
		)
		.await;
		assert!(
			res.is_err(),
			"no server at the endpoint -> Err after retries"
		);
	}

	// The token is frame 1. This pins that it is never frame 1 to a socket
	// somebody else owns: `/etc/hosts` is root's, and a client that reached
	// `connect` would fail with `Io` (ENOTSOCK) instead. `UntrustedEndpoint`
	// can only come from the owner check, which sits ahead of `connect` and so
	// ahead of `present_auth` — the whole point, since a check after the frame
	// has gone out is decoration. Skipped under an euid of 0, where nothing on
	// the filesystem is foreign and the case cannot fail.
	#[cfg(unix)]
	#[tokio::test]
	async fn a_foreign_owned_endpoint_is_refused_before_the_token_is_presented() {
		// SAFETY: `geteuid` cannot fail and touches no memory the caller owns.
		if unsafe { libc::geteuid() } == 0 || !std::path::Path::new("/etc/hosts").exists() {
			return;
		}
		let err = KernRpcClient::connect_endpoint_with_retry(
			&Endpoint::Unix(std::path::PathBuf::from("/etc/hosts")),
			&AuthReq::new("t"),
			3,
			Duration::from_millis(1),
		)
		.await
		.err()
		.expect("a foreign endpoint never yields a client");
		assert!(
			matches!(err, AdapterError::UntrustedEndpoint(_)),
			"refused by the owner check, before any frame: {err}"
		);
	}
}
