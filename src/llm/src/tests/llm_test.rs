//! Tests extracted from llm.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn strip_think_recovers_answer_after_leaked_reasoning() {
		assert_eq!(strip_think("plain answer"), "plain answer");
		assert_eq!(strip_think("<think>hmm</think>yes"), "yes");
		assert_eq!(strip_think("leaked reasoning</think>yes"), "yes");
		assert_eq!(strip_think("a</think>b</think>final"), "final");
		assert_eq!(strip_think("answer<think>unclosed trailing"), "answer");
		assert_eq!(strip_think("<think>only reasoning"), "");
	}

	#[test]
	fn permanent_client_errors_do_not_retry_single() {
		assert!(!should_retry_single(&LlmError::Api {
			status: 400,
			body: String::new()
		}));
		assert!(!should_retry_single(&LlmError::Api {
			status: 401,
			body: String::new()
		}));
		assert!(!should_retry_single(&LlmError::EmptyCompletion));
	}

	#[test]
	fn transient_and_empty_batch_retry_single() {
		assert!(should_retry_single(&LlmError::Api {
			status: 429,
			body: String::new()
		}));
		assert!(should_retry_single(&LlmError::Api {
			status: 503,
			body: String::new()
		}));
		assert!(should_retry_single(&LlmError::EmptyEmbedding));
	}

	#[test]
	fn local_ollama_markers_match_loopback_and_default_port() {
		assert!(is_local_ollama("http://localhost"));
		assert!(is_local_ollama("http://127.0.0.1:9999"));
		assert!(is_local_ollama("http://ollama:11434"));
		assert!(!is_local_ollama("https://api.openai.com"));
		assert!(!is_local_ollama("http://notlocalhost.com"));
	}

	#[test]
	fn is_local_url_accepts_local_hosts() {
		// loopback variants
		assert!(is_local_url("http://localhost"));
		assert!(is_local_url("http://127.0.0.1"));
		assert!(is_local_url("http://127.1.2.3:11434"));
		assert!(is_local_url("http://[::1]:8080"));
		// RFC1918
		assert!(is_local_url("http://10.0.0.1"));
		assert!(is_local_url("http://172.16.0.1"));
		assert!(is_local_url("http://172.27.176.1:11434")); // WSL2 gateway used by the LoCoMo run
		assert!(is_local_url("http://172.31.255.255"));
		assert!(is_local_url("http://192.168.1.1/embed"));
		// link-local
		assert!(is_local_url("http://169.254.0.1"));
		// ollama host / default port (reuses is_local_ollama)
		assert!(is_local_url("http://ollama:11434"));
		assert!(is_local_url("http://ollama"));
		assert!(is_local_url("http://anything:11434"));
	}

	#[test]
	fn is_local_url_rejects_public_hosts() {
		assert!(!is_local_url("https://api.openai.com"));
		assert!(!is_local_url("http://example.com"));
		assert!(!is_local_url("http://203.0.113.5"));
		assert!(!is_local_url("https://1.2.3.4/v1"));
		assert!(!is_local_url("http://8.8.8.8"));
		// 172.32 is outside the RFC1918 /12, not local
		assert!(!is_local_url("http://172.32.0.1"));
	}

	#[test]
	fn explicit_v1_suffix_forces_openai_compat_even_on_localhost() {
		assert!(wants_native("http://localhost:11434"));
		assert!(wants_native("http://localhost:11434/"));
		assert!(!wants_native("http://localhost:8000/v1"));
		assert!(!wants_native("http://127.0.0.1:8000/v1/"));
		assert!(!wants_native("https://api.openai.com/v1"));
	}

	#[tokio::test]
	async fn embed_falls_back_to_single_on_transient_batch_error() {
		use axum::http::StatusCode;
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|body: axum::Json<Value>| async move {
				if body.0["input"].is_array() {
					(
						StatusCode::SERVICE_UNAVAILABLE,
						axum::Json(serde_json::json!({ "error": "busy" })),
					)
				} else {
					(
						StatusCode::OK,
						axum::Json(serde_json::json!({ "embeddings": [[1.0, 2.0, 3.0]] })),
					)
				}
			}),
		);
		let (url, _server) = test_support::spawn_http(app).await;
		let client = Client::new_embed_only(&url, "m", "");
		let v = client
			.embed("hello")
			.await
			.expect("transient batch -> single retry succeeds");
		assert_eq!(v, vec![1.0, 2.0, 3.0]);
	}

	#[tokio::test]
	async fn embed_falls_back_to_single_on_empty_batch_response() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|body: axum::Json<Value>| async move {
				if body.0["input"].is_array() {
					axum::Json(serde_json::json!({ "embeddings": [] }))
				} else {
					axum::Json(serde_json::json!({ "embeddings": [[9.0]] }))
				}
			}),
		);
		let (url, _server) = test_support::spawn_http(app).await;
		let client = Client::new_embed_only(&url, "m", "");
		let v = client
			.embed("x")
			.await
			.expect("empty batch -> single retry succeeds");
		assert_eq!(v, vec![9.0]);
	}

	#[tokio::test]
	async fn embed_propagates_permanent_batch_error_without_retry() {
		use axum::http::StatusCode;
		use std::sync::atomic::{AtomicUsize, Ordering};
		let hits = Arc::new(AtomicUsize::new(0));
		let h = hits.clone();
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(move |_body: axum::Json<Value>| {
				let h = h.clone();
				async move {
					h.fetch_add(1, Ordering::SeqCst);
					(
						StatusCode::BAD_REQUEST,
						axum::Json(serde_json::json!({ "error": "bad model" })),
					)
				}
			}),
		);
		let (url, _server) = test_support::spawn_http(app).await;
		let client = Client::new_embed_only(&url, "m", "");
		let err = client.embed("hello").await.unwrap_err();
		assert!(
			matches!(err, LlmError::Api { status: 400, .. }),
			"permanent error propagates, got {err:?}"
		);
		assert_eq!(
			hits.load(Ordering::SeqCst),
			1,
			"no wasted single retry on a permanent error"
		);
	}

	// A chat endpoint that answers however the test needs it to fail. Kept here
	// rather than in `test_support` because only the completion leg's failure
	// channel cares about the shapes.
	fn chat_app(mode: &'static str) -> axum::Router {
		use axum::http::StatusCode;
		use axum::response::IntoResponse;
		axum::Router::new().route(
			"/api/chat",
			axum::routing::post(move |_b: axum::Json<Value>| async move {
				match mode {
					"hang" => {
						std::future::pending::<()>().await;
						unreachable!()
					}
					// A real gateway answers 5xx with an HTML page, not JSON — and
					// `LlmError::Api` renders the whole body, so this is what would
					// end up on a health line if nothing bounded it.
					"500" => (
						StatusCode::INTERNAL_SERVER_ERROR,
						format!(
							"<!DOCTYPE HTML>\n<html><body>{}</body></html>",
							"x".repeat(400)
						),
					)
						.into_response(),
					// A well-formed reply carrying nothing — the weak model.
					_ => axum::Json(serde_json::json!({
						"message": { "role": "assistant", "content": "" },
						"done": true
					}))
					.into_response(),
				}
			}),
		)
	}

	// The three outcomes item 30 says were one empty string. Deltas, never
	// absolutes: the counter is a process-global static, so `cargo test` running
	// the whole crate in one process sees every other test's failures too, and an
	// `assert_eq!(complete_failed(), 1)` is green under nextest and red under it.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_failed_completion_is_counted_and_named_instead_of_erased() {
		let mut named: Vec<(&str, String)> = Vec::new();
		for (mode, want) in [
			("hang", "transient: HTTP error"),
			("refused", "transient: HTTP error"),
			("500", "transient: API error (500)"),
			("empty", "permanent: empty completion response"),
		] {
			// A closed port for the refusal, a served one for the rest.
			let (url, _server) = match mode {
				"refused" => ("http://127.0.0.1:1".to_string(), None),
				_ => {
					let (u, h) = test_support::spawn_http(chat_app(mode)).await;
					(u, Some(h))
				}
			};
			// One second, not ten minutes: the same key the config now sets, which
			// is also what makes the hang case finish inside a test.
			let f = Client::new(Endpoint::new(&url, "m", ""), Endpoint::default())
				.with_timeout_secs(1)
				.complete_func();

			let before = complete_failed();
			let out = tokio::task::spawn_blocking(move || f("say something"))
				.await
				.unwrap();

			assert_eq!(out, "", "{mode}: the caller's contract is unchanged");
			assert_eq!(
				complete_failed() - before,
				1,
				"{mode}: exactly one failure counted"
			);
			let last = last_complete_failure();
			assert!(
				last.starts_with(want),
				"{mode}: the surface must name the failure, got {last:?}"
			);
			// It has to fit on a health line: an endpoint's 5xx body is an HTML
			// page, and pasting it whole would push every other line off screen.
			assert!(!last.contains('\n'), "{mode}: one line only, got {last:?}");
			assert!(
				last.chars().count() <= REASON_MAX_CHARS + 16,
				"{mode}: unbounded reason, got {} chars",
				last.chars().count()
			);
			named.push((mode, last));
		}

		// The point of the item, not merely that each is named: no two read alike.
		// A surface that printed one string for all four would satisfy every
		// assertion above and none of item 30.
		for (i, (a_mode, a)) in named.iter().enumerate() {
			for (b_mode, b) in &named[i + 1..] {
				assert_ne!(a, b, "{a_mode} and {b_mode} must not read alike");
			}
		}
	}

	// The control for the test above: a model that answers with prose is not an
	// endpoint failure, and must not raise the counter that says the endpoint is
	// at fault. This is the case `record_stuck` could not distinguish.
	// ROADMAP item 84: `complete` retries a transient (5xx) before surfacing
	// the failure — the distill leg should not re-queue a whole transcript on a
	// gateway blip. The first call 500s, the second answers; the completion
	// returns the content, not "".
	#[tokio::test(flavor = "multi_thread")]
	async fn complete_retries_a_transient_5xx_then_succeeds() {
		use axum::response::IntoResponse;
		use std::sync::atomic::{AtomicU32, Ordering};
		use std::sync::Arc;
		let calls = Arc::new(AtomicU32::new(0));
		let calls2 = calls.clone();
		let app = axum::Router::new().route(
			"/api/chat",
			axum::routing::post(move |_b: axum::Json<Value>| async move {
				let n = calls2.fetch_add(1, Ordering::SeqCst);
				if n == 0 {
					(
						axum::http::StatusCode::INTERNAL_SERVER_ERROR,
						"blip".to_string(),
					)
						.into_response()
				} else {
					axum::Json(serde_json::json!({
						"message": { "role": "assistant", "content": "recovered" },
						"done": true
					}))
					.into_response()
				}
			}),
		);
		let (url, _server) = test_support::spawn_http(app).await;
		let client = Client::new(Endpoint::new(&url, "m", ""), Endpoint::default());
		let out = client.complete("say something").await.unwrap();
		assert_eq!(out, "recovered", "the retry reached the answering call");
		assert_eq!(calls.load(Ordering::SeqCst), 2, "one 500 + one ok");
		assert_eq!(
			complete_failed(),
			0,
			"a recovered completion is not a failure"
		);
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn a_weak_model_that_answers_is_not_counted_as_a_failure() {
		let app = axum::Router::new().route(
			"/api/chat",
			axum::routing::post(|_b: axum::Json<Value>| async move {
				axum::Json(serde_json::json!({
					"message": { "role": "assistant", "content": "I am not sure." },
					"done": true
				}))
			}),
		);
		let (url, _server) = test_support::spawn_http(app).await;
		let f = Client::new(Endpoint::new(&url, "m", ""), Endpoint::default()).complete_func();

		let before = complete_failed();
		let out = tokio::task::spawn_blocking(move || f("say something"))
			.await
			.unwrap();

		assert_eq!(out, "I am not sure.");
		assert_eq!(
			complete_failed() - before,
			0,
			"prose is the model's answer, not the endpoint's fault"
		);
	}
}
