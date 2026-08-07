//! The kern RPC: auth (token presentation, constant-time verify), the
//! tolerant-decode DTOs (the live attach → detect-stale → auto-restart
//! handshake must talk to daemons from older builds), the `service!`-generated
//! client/server pair, and the local attach client.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::transport::typed::{AdapterError, Channel, JsonEnvelopeCodec};

use crate::transport::http::ct_eq;

/// The one frame a caller sends before any `KernRpc` method is reachable.
///
/// `token` is the per-graph secret the daemon minted (`resolve_mcp_token`) —
/// the same `mcp-token` the HTTP surface demands, never a second one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthReq {
	pub token: String,
}

// One message for every refusal. A missing frame, a malformed frame and a wrong
// token must read identically, or the reply becomes an oracle that tells a
// caller how far it got.
const REFUSED: &str = "kern.sock: unauthenticated";

/// The cap on the one frame an unproven peer may send. A real `AuthReq` is
/// `{"auth":{"token":"<64 hex>"}}` — under 100 bytes. 1 KiB is an order of
/// magnitude under `FramedRead`'s own 8 KiB starting buffer, so the refusal
/// lands on the first decode, before that buffer has doubled even once.
const AUTH_FRAME_MAX: usize = 1024;

/// The deadline on that same frame. Every real client writes the token in the
/// same breath as the connect (`KernRpcClient::connect_local`), so the handshake
/// is a microsecond conversation over a local socket. Five seconds is four
/// orders of magnitude of slack for a loaded machine and still finite, which is
/// the whole difference: the authenticated path waits forever by design, and a
/// peer that has proven nothing does not get that.
const AUTH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

impl AuthReq {
	pub fn new(token: impl Into<String>) -> Self {
		Self {
			token: token.into(),
		}
	}
}

/// Client half: present the token, then wait for the daemon's verdict.
/// Anything but an explicit `ok: true` is a refusal.
pub async fn present_auth(
	channel: &mut Channel<JsonEnvelopeCodec>,
	auth: &AuthReq,
) -> Result<(), AdapterError> {
	let frame = serde_json::json!({ "auth": auth });
	channel.send(frame).await?;
	match channel.recv().await {
		Ok(Some(reply)) if reply.pointer("/auth/ok").and_then(Value::as_bool) == Some(true) => Ok(()),
		_ => Err(AdapterError::Unauthenticated(REFUSED.to_string())),
	}
}

/// Server half: read the caller's auth frame and verify it.
///
/// Every other outcome is a refusal — EOF, a codec error, a frame that is not
/// an auth frame, a token that does not match, and an `expected` that is itself
/// empty. There is no branch here that returns `Ok` without having compared a
/// non-empty secret, which is the whole point: a gate that fails open reads as
/// protection while being none.
///
/// This read is the only one an unproven peer can reach, so it is also the only
/// one that is bounded in both directions: `AUTH_FRAME_MAX` bytes and
/// `AUTH_DEADLINE` of patience, both lifted the moment the frame is in hand.
pub async fn verify_auth(
	channel: &mut Channel<JsonEnvelopeCodec>,
	expected: &str,
) -> Result<(), AdapterError> {
	channel
		.decoder_mut()
		.set_max_frame_len(Some(AUTH_FRAME_MAX));
	let read = tokio::time::timeout(AUTH_DEADLINE, channel.recv()).await;
	channel.decoder_mut().set_max_frame_len(None);
	// A peer that ran out the clock is dropped without a word. Every other
	// refusal answers so a misconfigured client reports "refused" instead of a
	// bare EOF — but this one never spoke, so there is no client to inform, and
	// the reply would be a free liveness probe that also names the deadline.
	let Ok(read) = read else {
		return Err(AdapterError::Unauthenticated(REFUSED.to_string()));
	};
	let req = match read {
		Ok(Some(frame)) => frame
			.get("auth")
			.cloned()
			.and_then(|v| serde_json::from_value::<AuthReq>(v).ok()),
		_ => None,
	};
	match req {
		Some(req) if !expected.is_empty() && ct_eq(req.token.as_bytes(), expected.as_bytes()) => {
			channel
				.send(serde_json::json!({ "auth": { "ok": true } }))
				.await?;
			Ok(())
		}
		_ => {
			// Best-effort: say no out loud so a misconfigured client reports a
			// refusal instead of a bare EOF. The refusal stands either way.
			let _ = channel
				.send(serde_json::json!({ "auth": { "ok": false, "error": REFUSED } }))
				.await;
			Err(AdapterError::Unauthenticated(REFUSED.to_string()))
		}
	}
}

#[cfg(test)]
mod tests {
	use std::pin::Pin;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::Arc;
	use std::task::{Context as TaskContext, Poll};

	use tokio::io::{AsyncRead, ReadBuf};

	use super::*;
	use crate::transport::typed::InprocAdapter;
	use crate::transport::typed::{Adapter, DynRead, DynWrite};

	fn pair() -> (Channel<JsonEnvelopeCodec>, Channel<JsonEnvelopeCodec>) {
		let (a, b) = InprocAdapter::pair();
		(
			Channel::new(a, JsonEnvelopeCodec::new()),
			Channel::new(b, JsonEnvelopeCodec::new()),
		)
	}

	/// A peer that writes garbage and never a newline, counting what the server
	/// actually takes from it. Nothing is ever consumed from `FramedRead`'s
	/// buffer while `decode` returns `None`, so this count *is* the buffer's
	/// size — which is what makes it evidence about allocation and not just
	/// about the verdict.
	///
	/// Finite, so the unfixed case fails on a number rather than hanging.
	struct Flood {
		left: usize,
		taken: Arc<AtomicUsize>,
	}

	impl AsyncRead for Flood {
		fn poll_read(
			mut self: Pin<&mut Self>,
			_cx: &mut TaskContext<'_>,
			buf: &mut ReadBuf<'_>,
		) -> Poll<std::io::Result<()>> {
			let n = self.left.min(buf.remaining());
			if n > 0 {
				buf.put_slice(&vec![b'x'; n]);
				self.left -= n;
				self.taken.fetch_add(n, Ordering::SeqCst);
			}
			Poll::Ready(Ok(()))
		}
	}

	impl Adapter for Flood {
		fn split(self: Box<Self>) -> (DynRead, DynWrite) {
			(Box::new(*self), Box::new(tokio::io::sink()))
		}
	}

	#[test]
	fn ct_eq_agrees_with_plain_equality_including_the_prefix_case() {
		assert!(ct_eq(b"abc", b"abc"));
		assert!(!ct_eq(b"abc", b"abd"));
		assert!(!ct_eq(b"abc", b"ab"), "a shared prefix is not a match");
		assert!(!ct_eq(b"", b"a"));
		assert!(ct_eq(b"", b""), "equal lengths, no differing bytes");
	}

	#[tokio::test]
	async fn the_right_token_verifies() {
		let (mut server, mut client) = pair();
		let task = tokio::spawn(async move { verify_auth(&mut server, "s3cret").await });
		present_auth(&mut client, &AuthReq::new("s3cret"))
			.await
			.expect("the right token is accepted");
		task.await.unwrap().expect("the server accepted it too");
	}

	// `s3crey` is the load-bearing case, not `guess`. A wrong token of a
	// *different* length is refused by `ct_eq`'s length check alone, so a suite
	// that only ever offers one never runs the byte compare at all — delete the
	// compare's body and every such test still passes. `s3crey` is the same
	// length as `s3cret` and differs in the last byte, so it can only be refused
	// by the compare, and only by one that reads to the end.
	#[tokio::test]
	async fn a_wrong_token_is_refused_on_both_halves() {
		for offered in ["guess", "s3crey"] {
			let (mut server, mut client) = pair();
			let task = tokio::spawn(async move { verify_auth(&mut server, "s3cret").await });
			let out = present_auth(&mut client, &AuthReq::new(offered)).await;
			assert!(
				matches!(out, Err(AdapterError::Unauthenticated(_))),
				"the client must learn it was refused, not that nothing was there (offered {offered:?})"
			);
			assert!(task.await.unwrap().is_err(), "offered {offered:?}");
		}
	}

	#[tokio::test]
	async fn a_frame_that_is_not_an_auth_frame_is_refused() {
		let (mut server, mut client) = pair();
		let task = tokio::spawn(async move { verify_auth(&mut server, "s3cret").await });
		client
			.send(serde_json::json!({"id": 1, "method": "call_tool", "params": {}}))
			.await
			.unwrap();
		assert!(
			task.await.unwrap().is_err(),
			"a caller that skips the handshake is a caller with no identity"
		);
	}

	#[tokio::test]
	async fn an_auth_frame_with_no_token_field_is_refused() {
		let (mut server, mut client) = pair();
		let task = tokio::spawn(async move { verify_auth(&mut server, "s3cret").await });
		client.send(serde_json::json!({"auth": {}})).await.unwrap();
		assert!(task.await.unwrap().is_err(), "no token is not a token");
	}

	#[tokio::test]
	async fn a_hung_up_caller_is_refused_rather_than_admitted() {
		let (mut server, client) = pair();
		drop(client);
		assert!(
			verify_auth(&mut server, "s3cret").await.is_err(),
			"EOF before the handshake must fail closed"
		);
	}

	// The size half. There is no length prefix on this wire, so nothing declares
	// how big the frame will be — `FramedRead` just keeps reading and doubling
	// until a newline arrives, and the cap is the only thing that can stop it.
	//
	// The assertion is on *bytes the daemon took*, not on the verdict: an
	// unfixed daemon refuses this too, at EOF, having buffered all 16 MiB first.
	// A test that only checked the refusal would be green through the whole
	// defect.
	#[tokio::test]
	async fn an_endless_pre_auth_frame_is_refused_without_being_buffered() {
		let taken = Arc::new(AtomicUsize::new(0));
		let mut server = Channel::new(
			Flood {
				left: 16 * 1024 * 1024,
				taken: taken.clone(),
			},
			JsonEnvelopeCodec::new(),
		);
		assert!(
			verify_auth(&mut server, "s3cret").await.is_err(),
			"16 MiB of 'x' is not a token"
		);
		let took = taken.load(Ordering::SeqCst);
		assert!(
			took <= 64 * 1024,
			"the daemon buffered {took} bytes from a peer that has proven nothing"
		);
	}

	// The patience half, on a clock that costs no wall time. Two timers exist
	// here: `AUTH_DEADLINE` inside `verify_auth` and this outer one. If the
	// pre-auth read has no deadline of its own, the outer one is the only timer
	// and it is what fires — which is the assertion.
	#[tokio::test(start_paused = true)]
	async fn a_peer_that_opens_and_says_nothing_is_dropped_by_the_deadline() {
		let (mut server, _client) = pair();
		let verdict = tokio::time::timeout(AUTH_DEADLINE * 4, verify_auth(&mut server, "s3cret"))
			.await
			.expect("a silent peer held the pre-auth read open past its deadline");
		assert!(
			matches!(verdict, Err(AdapterError::Unauthenticated(_))),
			"a peer that never spoke is unauthenticated, not an i/o fault"
		);
	}

	// The cap must not be so tight it refuses the real client. `s3cret` is six
	// bytes and proves nothing about the number chosen — a token the daemon
	// actually mints is 64 hex characters (`mint_token`), which is what the
	// budget was sized against.
	#[tokio::test]
	async fn a_real_sized_token_frame_still_fits_under_the_cap() {
		let token = "0".repeat(64);
		let (mut server, mut client) = pair();
		let expected = token.clone();
		let task = tokio::spawn(async move { verify_auth(&mut server, &expected).await });
		present_auth(&mut client, &AuthReq::new(token))
			.await
			.expect("the token the daemon mints must fit the frame it is sent in");
		task.await.unwrap().expect("the server accepted it too");
	}

	// The daemon-side degenerate case. If the secret could not be read, the
	// expected token is empty — and an empty expectation must reject everyone,
	// including a caller that helpfully sends an empty token.
	#[tokio::test]
	async fn an_empty_expected_token_authenticates_nobody() {
		for offered in ["", "anything"] {
			let (mut server, mut client) = pair();
			let task = tokio::spawn(async move { verify_auth(&mut server, "").await });
			let _ = present_auth(&mut client, &AuthReq::new(offered)).await;
			assert!(
				task.await.unwrap().is_err(),
				"a daemon with no secret must serve nobody, not everybody (offered {offered:?})"
			);
		}
	}
}

use std::time::Duration;

use crate::transport::typed::{connect_kern, Endpoint};

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
mod client_tests {
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

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModeWeightsHealth {
	#[serde(default)]
	pub content: f64,
	#[serde(default)]
	pub reason: f64,
	#[serde(default)]
	pub edge: f64,
}

// Active RRF config (`RetrievalConfig.rrf_k` / `rrf_global_weight` / the three
// `ModeWeights`) plus the remaining active retrieval knobs (`seed_k`,
// `mmr_enabled`, `lexical_enabled`, `pagerank_enabled`), preset-owned. Zeroed
// from older daemons (ROADMAP item 66 measurement half).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RetrievalHealth {
	#[serde(default)]
	pub rrf_k: f64,
	#[serde(default)]
	pub rrf_global_weight: f64,
	#[serde(default)]
	pub weights_content: ModeWeightsHealth,
	#[serde(default)]
	pub weights_reason: ModeWeightsHealth,
	#[serde(default)]
	pub weights_hybrid: ModeWeightsHealth,
	#[serde(default)]
	pub seed_k: usize,
	#[serde(default)]
	pub mmr_enabled: bool,
	#[serde(default)]
	pub lexical_enabled: bool,
	#[serde(default)]
	pub pagerank_enabled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ShutdownRes {
	pub ok: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HealthRes {
	pub ok: bool,
	#[serde(default)]
	pub data_dir: String,
	#[serde(default)]
	pub kerns: u64,
	#[serde(default)]
	pub entities: u64,
	// Ms since the last real tool call (health polls excluded). 0 from older
	// daemons that predate the field — the hub treats that as "never idle".
	#[serde(default)]
	pub idle_ms: u64,
	#[serde(default)]
	pub queue_depth: u64,
	#[serde(default)]
	pub tasks_done: u64,
	// Lifetime mean over `tasks_done`, not a recent window: it converges and
	// stops moving, so read it as a baseline, never as current load.
	#[serde(default)]
	pub task_avg_ms: u64,
	// Degraded maintenance. A panic killed its task; a failure ended it early and
	// re-enqueues forever. Empty string = none recorded, including on old daemons.
	#[serde(default)]
	pub task_panics: u64,
	#[serde(default)]
	pub last_task_panic: String,
	#[serde(default)]
	pub task_failures: u64,
	#[serde(default)]
	pub last_task_failure: String,
	// Store health: cold rows the FIFO cap dropped, and the embedding stamp the
	// index was built with. `embed_mismatch` means the live model is not that one.
	#[serde(default)]
	pub cold_evicted: u64,
	#[serde(default)]
	pub embed_model: String,
	#[serde(default)]
	pub embed_dim: u64,
	#[serde(default)]
	pub embed_mismatch: bool,
	// Fail-open degradations. Each is a path that returns something rather than
	// erroring, so the count is the only way to tell a degraded result from a
	// good one: queries the dimension guard dropped, deliveries that bypassed
	// `min_deliver_score` because nothing cleared it, and entities GC could not
	// age because their timestamp is in the future.
	#[serde(default)]
	pub query_dim_rejected: u64,
	#[serde(default)]
	pub below_floor_deliveries: u64,
	#[serde(default)]
	pub clock_skew_skips: u64,
	#[serde(default)]
	pub ingest_dropped_chunks: u64,
	#[serde(default)]
	pub remote_cap_dropped: u64,
	#[serde(default)]
	pub unspilled_drops: u64,
	#[serde(default)]
	pub ingest_queue_refused: u64,
	// Jobs parked in the ingest RAM queue right now — a gauge, not a counter.
	#[serde(default)]
	pub ingest_queue_depth: u64,
	// Gini over resident entities' access counts: 0.0 = uniform (converged),
	// →1.0 = one entity holds all access. 0.0 from older daemons (item 62).
	#[serde(default)]
	pub gini_access: f64,
	// The resident-kern cap: 0 = old daemon / unset, `u64::MAX` = uncapped
	// (`KERN_CAP_DISABLED`). A live bound is >= 1 (item 83).
	#[serde(default)]
	pub max_kerns: u64,
	// Propagations the trainer refused past its queue cap. Those kerns keep the
	// `gnn_vector` they already had, so the count is the only trace.
	#[serde(default)]
	pub gnn_train_refused: u64,
	// Supersede chains that exceeded `SUPERSEDE_CHAIN_HOP_THRESHOLD` on one
	// `external_id` (ROADMAP item 58 trigger #1). 0 from older daemons.
	#[serde(default)]
	pub supersede_chain_depth_exceeded: u64,
	// The largest resident kern's entity count (ROADMAP item 83). 0 from older
	// daemons.
	#[serde(default)]
	pub largest_kern_entities: usize,
	// Gini over resident kern sizes (ROADMAP item 83). 0.0 from older daemons.
	#[serde(default)]
	pub gini_kern_sizes: f64,
	// Active heat retention half-life (`HeatConfig.half_life_secs`, the one
	// `Preset::apply` sets — relaxed=30d / medium=7d / tight=3d, never a config
	// edit). 0 from older daemons (ROADMAP item 62 `kern://health` surfacing).
	#[serde(default)]
	pub heat_half_life_secs: u64,
	// QBST recency half-life (`RetrievalConfig.qbst_recency_half_life_secs`,
	// the 24h ranking-freshness signal). 0 from older daemons (ROADMAP item 55).
	#[serde(default)]
	pub qbst_recency_half_life_secs: u64,
	// Active RRF config + mode blends (ROADMAP item 66 measurement half).
	// Zeroed from older daemons.
	#[serde(default)]
	pub retrieval: RetrievalHealth,
	// Active preset name (`Config.preset`, `Preset::apply` is its only writer).
	// Empty from older daemons (ROADMAP item 87 measurement half).
	#[serde(default)]
	pub preset: String,
	// Active source-trust map (`RetrievalConfig.source_trust`, keyed on
	// `Source::scheme()` — file/ticket/session/agent/inline). Empty from
	// older daemons and from a configless kern (ROADMAP item 20 measurement
	// half).
	#[serde(default)]
	pub source_trust: BTreeMap<String, f64>,
	// Active ingest dedup config (`IngestConfig.dedup_threshold` + the
	// per-kind `dedup_threshold_by_kind` array, shipped 2026-07-23 by item 48
	// beside). `0.0` / `[None; 5]` from older daemons and from a configless
	// kern (ROADMAP item 48 measurement half). The array is indexed by
	// `EntityKind as u8` (Fact=0 .. Conclusion=4); `None` falls back to the
	// global threshold.
	#[serde(default)]
	pub ingest_dedup_threshold: f64,
	#[serde(default)]
	pub ingest_dedup_threshold_by_kind: [Option<f64>; 5],
	// Completions that failed on the reason endpoint, and the last one in words.
	// The blocking bridge hands its caller `""` for every failure, so the count
	// is what separates a dead endpoint from a model with nothing to say, and the
	// string is what separates a timeout from a refusal from an empty body.
	#[serde(default)]
	pub llm_complete_failed: u64,
	#[serde(default)]
	pub last_llm_complete_failure: String,
	// Staleness identity. `build_id` fingerprints the running executable,
	// `config_id` the resolved config, so an edited kern.toml reads as stale
	// even when the binary did not move. Empty from daemons predating the
	// fields — and empty must never be treated as a mismatch, or every attach
	// to an older daemon would restart it.
	#[serde(default)]
	pub build_id: String,
	#[serde(default)]
	pub config_id: String,
	// Ms since the daemon booted. Guards the auto-restart against thrash when
	// two clients with different builds alternate. 0 = unknown, do not restart.
	#[serde(default)]
	pub uptime_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallToolReq {
	pub name: String,
	#[serde(default)]
	pub args: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CallToolRes {
	pub envelope: serde_json::Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ListToolsReq {}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ListToolsRes {
	pub tools: Vec<serde_json::Value>,
}

#[cfg(test)]
mod dto_serde_tests {
	use super::*;

	#[test]
	fn an_older_health_payload_without_queue_fields_still_deserializes() {
		let old = r#"{"ok":true,"data_dir":"/d","kerns":3,"entities":7,"idle_ms":42}"#;
		let h: HealthRes = serde_json::from_str(old).expect("append-only: old shape must decode");
		assert_eq!(h.kerns, 3);
		assert_eq!(h.idle_ms, 42);
		assert_eq!(h.queue_depth, 0, "absent field defaults, never errors");
		assert_eq!(h.tasks_done, 0);
		assert_eq!(h.task_avg_ms, 0);
		assert_eq!(h.task_panics, 0);
		assert!(h.last_task_panic.is_empty());
		assert_eq!(h.task_failures, 0);
		assert!(h.last_task_failure.is_empty());
		assert_eq!(h.cold_evicted, 0);
		assert!(h.embed_model.is_empty());
		assert_eq!(h.embed_dim, 0);
		assert!(!h.embed_mismatch, "an old daemon is not a mismatching one");
		assert_eq!(h.query_dim_rejected, 0);
		assert_eq!(h.below_floor_deliveries, 0);
		assert_eq!(
			h.clock_skew_skips, 0,
			"an old daemon reports no degradation"
		);
		assert_eq!(h.ingest_dropped_chunks, 0);
		assert_eq!(h.remote_cap_dropped, 0);
		assert_eq!(h.unspilled_drops, 0);
		assert_eq!(h.ingest_queue_refused, 0);
		assert_eq!(h.ingest_queue_depth, 0);
		assert_eq!(h.gnn_train_refused, 0);
		assert_eq!(h.llm_complete_failed, 0);
		assert!(h.last_llm_complete_failure.is_empty());
		assert!(h.build_id.is_empty(), "unknown build, not a stale one");
		assert!(h.config_id.is_empty());
		assert_eq!(h.uptime_ms, 0);
		assert_eq!(h.largest_kern_entities, 0);
		assert!((h.gini_kern_sizes - 0.0).abs() < 1e-12);
		assert!((h.retrieval.rrf_k - 0.0).abs() < 1e-12);
		assert!((h.retrieval.rrf_global_weight - 0.0).abs() < 1e-12);
		assert!((h.retrieval.weights_content.content - 0.0).abs() < 1e-12);
		assert_eq!(h.retrieval.seed_k, 0);
		assert!(!h.retrieval.mmr_enabled);
		assert!(!h.retrieval.lexical_enabled);
		assert!(!h.retrieval.pagerank_enabled);
		assert!(h.preset.is_empty(), "an old daemon reports no preset name");
		assert!(
			h.source_trust.is_empty(),
			"an old daemon reports no source-trust map"
		);
		assert!(
			(h.ingest_dedup_threshold - 0.0).abs() < 1e-12,
			"an old daemon reports no ingest dedup threshold"
		);
		assert!(
			h.ingest_dedup_threshold_by_kind.iter().all(Option::is_none),
			"an old daemon reports no per-kind dedup overrides"
		);

		let ancient = r#"{"ok":true}"#;
		let h2: HealthRes = serde_json::from_str(ancient).expect("only `ok` is required");
		assert!(h2.ok);
		assert_eq!(h2.task_avg_ms, 0);
	}

	#[test]
	fn every_health_field_round_trips_through_json() {
		let src = HealthRes {
			ok: true,
			data_dir: "/d".into(),
			kerns: 1,
			entities: 2,
			idle_ms: 3,
			queue_depth: 4,
			tasks_done: 5,
			task_avg_ms: 6,
			task_panics: 7,
			last_task_panic: "GnnPropagate[k]: boom".into(),
			task_failures: 8,
			last_task_failure: "GnnPropagate[k]: train epoch 0 forward".into(),
			cold_evicted: 9,
			embed_model: "qwen3".into(),
			embed_dim: 1024,
			embed_mismatch: true,
			query_dim_rejected: 11,
			below_floor_deliveries: 12,
			clock_skew_skips: 13,
			ingest_dropped_chunks: 14,
			remote_cap_dropped: 15,
			unspilled_drops: 16,
			ingest_queue_refused: 17,
			ingest_queue_depth: 21,
			gini_access: 0.42,
			max_kerns: 128,
			gnn_train_refused: 18,
			supersede_chain_depth_exceeded: 22,
			largest_kern_entities: 99,
			gini_kern_sizes: 0.42,
			heat_half_life_secs: 2592000,
			qbst_recency_half_life_secs: 86400,
			retrieval: RetrievalHealth {
				rrf_k: 60.0,
				rrf_global_weight: 0.5,
				weights_content: ModeWeightsHealth {
					content: 0.7,
					reason: 0.2,
					edge: 0.1,
				},
				weights_reason: ModeWeightsHealth {
					content: 0.1,
					reason: 0.8,
					edge: 0.1,
				},
				weights_hybrid: ModeWeightsHealth {
					content: 0.5,
					reason: 0.3,
					edge: 0.2,
				},
				seed_k: 30,
				mmr_enabled: false,
				lexical_enabled: true,
				pagerank_enabled: true,
			},
			preset: "tight".into(),
			source_trust: BTreeMap::from([("file".to_string(), 0.8), ("ticket".to_string(), 0.9)]),
			ingest_dedup_threshold: 0.95,
			ingest_dedup_threshold_by_kind: [Some(0.99), None, None, None, None],
			llm_complete_failed: 19,
			last_llm_complete_failure: "transient: HTTP error: operation timed out".into(),
			build_id: "a1b2c3d4e5f60718".into(),
			config_id: "0f1e2d3c4b5a6978".into(),
			uptime_ms: 90_000,
		};
		let back: HealthRes = serde_json::from_str(&serde_json::to_string(&src).unwrap()).unwrap();
		assert_eq!(back.task_panics, 7);
		assert_eq!(back.last_task_panic, src.last_task_panic);
		assert_eq!(back.task_failures, 8);
		assert_eq!(back.last_task_failure, src.last_task_failure);
		assert_eq!(back.cold_evicted, 9);
		assert_eq!(back.embed_model, "qwen3");
		assert_eq!(back.embed_dim, 1024);
		assert!(back.embed_mismatch);
		assert_eq!(back.query_dim_rejected, 11);
		assert_eq!(back.below_floor_deliveries, 12);
		assert_eq!(back.clock_skew_skips, 13);
		assert_eq!(back.ingest_dropped_chunks, 14);
		assert_eq!(back.remote_cap_dropped, 15);
		assert_eq!(back.unspilled_drops, 16);
		assert_eq!(back.ingest_queue_refused, 17);
		assert_eq!(back.ingest_queue_depth, 21);
		assert!((back.gini_access - 0.42).abs() < 1e-12);
		assert_eq!(back.max_kerns, 128);
		assert_eq!(back.gnn_train_refused, 18);
		assert_eq!(back.supersede_chain_depth_exceeded, 22);
		assert_eq!(back.largest_kern_entities, 99);
		assert!((back.gini_kern_sizes - 0.42).abs() < 1e-12);
		assert_eq!(back.heat_half_life_secs, 2592000);
		assert_eq!(back.qbst_recency_half_life_secs, 86400);
		assert_eq!(back.retrieval.rrf_k, 60.0);
		assert!((back.retrieval.rrf_global_weight - 0.5).abs() < 1e-12);
		assert!((back.retrieval.weights_content.content - 0.7).abs() < 1e-12);
		assert!((back.retrieval.weights_reason.reason - 0.8).abs() < 1e-12);
		assert!((back.retrieval.weights_hybrid.edge - 0.2).abs() < 1e-12);
		assert_eq!(back.retrieval.seed_k, 30);
		assert!(!back.retrieval.mmr_enabled);
		assert!(back.retrieval.lexical_enabled);
		assert!(back.retrieval.pagerank_enabled);
		assert_eq!(back.preset, "tight");
		assert_eq!(back.source_trust.get("file").copied().unwrap_or(0.0), 0.8);
		assert_eq!(back.source_trust.get("ticket").copied().unwrap_or(0.0), 0.9);
		assert!((back.ingest_dedup_threshold - 0.95).abs() < 1e-12);
		assert_eq!(
			back.ingest_dedup_threshold_by_kind,
			[Some(0.99), None, None, None, None]
		);
		assert_eq!(back.llm_complete_failed, 19);
		assert_eq!(
			back.last_llm_complete_failure,
			src.last_llm_complete_failure
		);
		assert_eq!(back.build_id, src.build_id);
		assert_eq!(back.config_id, src.config_id);
		assert_eq!(back.uptime_ms, 90_000);
	}
}

crate::transport::service! {
		pub trait KernRpc {
				async fn health() -> HealthRes;
				async fn shutdown() -> ShutdownRes;
				async fn call_tool(req: CallToolReq) -> CallToolRes;
				async fn list_tools(req: ListToolsReq) -> ListToolsRes;
		}
}
