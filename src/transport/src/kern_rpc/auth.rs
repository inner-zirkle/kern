use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::typed::{AdapterError, Channel, JsonEnvelopeCodec};

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

// Constant time over the compared bytes: a compare that returns at the first
// mismatch reports how long a shared prefix was.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
	if a.len() != b.len() {
		return false;
	}
	let mut diff = 0u8;
	for (x, y) in a.iter().zip(b) {
		diff |= x ^ y;
	}
	diff == 0
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
	use crate::typed::adapter::{Adapter, DynRead, DynWrite};
	use crate::typed::InprocAdapter;

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
