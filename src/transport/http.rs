//! Axum glue for MCP over HTTP: bearer-token auth (constant-time compare) in
//! front of the JSON-RPC dispatch endpoint.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;

use crate::transport::mcp::{dispatch, error_response};
use crate::transport::McpServer;

pub struct AppState<S> {
	server: Arc<S>,
	token: Option<Arc<str>>,
}

// Derived Clone would demand `S: Clone`; only the Arcs are cloned here.
impl<S> Clone for AppState<S> {
	fn clone(&self) -> Self {
		Self {
			server: self.server.clone(),
			token: self.token.clone(),
		}
	}
}

pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorized<S>(state: &AppState<S>, headers: &HeaderMap) -> bool {
	let Some(expected) = state.token.as_deref() else {
		return true;
	};
	headers
		.get(axum::http::header::AUTHORIZATION)
		.and_then(|v| v.to_str().ok())
		.and_then(|v| v.strip_prefix("Bearer "))
		.is_some_and(|got| ct_eq(got.trim().as_bytes(), expected.as_bytes()))
}

/// `token` gates every route. `None` leaves the surface open — callers binding
/// anything other than loopback are expected to pass one.
pub async fn serve_http<S>(server: Arc<S>, addr: &str, token: Option<&str>) -> std::io::Result<()>
where
	S: McpServer + Sync + 'static,
{
	let state = AppState {
		server,
		token: token.map(Arc::from),
	};
	let app = Router::new()
		.route("/mcp", post(handle_post::<S>))
		.route("/mcp", get(handle_get::<S>))
		.with_state(state);

	let listener = tokio::net::TcpListener::bind(addr).await?;
	axum::serve(listener, app).await?;
	Ok(())
}

async fn handle_post<S: McpServer + Sync + 'static>(
	State(state): State<AppState<S>>,
	headers: HeaderMap,
	body: String,
) -> impl IntoResponse {
	if !authorized(&state, &headers) {
		let resp = error_response(serde_json::Value::Null, -32001, "unauthorized");
		return (StatusCode::UNAUTHORIZED, axum::Json(resp));
	}
	let server = state.server;
	// A missing Content-Type is allowed (many MCP clients omit it); only an
	// explicit non-JSON one is rejected.
	if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
		let is_json = ct
			.to_str()
			.map(|s| s.trim_start().starts_with("application/json"))
			.unwrap_or(false);
		if !is_json {
			let resp = error_response(
				serde_json::Value::Null,
				-32700,
				"content-type must be application/json",
			);
			return (StatusCode::OK, axum::Json(resp));
		}
	}

	let frame: serde_json::Value = match serde_json::from_str(&body) {
		Ok(v) => v,
		Err(e) => {
			let resp = error_response(
				serde_json::Value::Null,
				-32700,
				&format!("parse error: {e}"),
			);
			return (StatusCode::OK, axum::Json(resp));
		}
	};

	match dispatch(server.as_ref(), &frame) {
		Some(resp) => (StatusCode::OK, axum::Json(resp)),
		None => (StatusCode::ACCEPTED, axum::Json(serde_json::Value::Null)),
	}
}

async fn handle_get<S: McpServer + Sync + 'static>(
	State(state): State<AppState<S>>,
	headers: HeaderMap,
) -> axum::response::Response {
	if !authorized(&state, &headers) {
		return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
	}
	let stream = async_stream::stream! {
		loop {
			tokio::time::sleep(std::time::Duration::from_secs(25)).await;
			yield Ok::<Event, std::convert::Infallible>(Event::default().comment("keepalive"));
		}
	};
	Sse::new(stream).into_response()
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::http::header::CONTENT_TYPE;
	use serde_json::{json, Value};

	struct MockServer;
	impl McpServer for MockServer {
		fn tools_list(&self) -> Vec<crate::transport::ToolSchema> {
			vec![crate::transport::ToolSchema {
				name: "add".into(),
				description: Some("a+b".into()),
				input_schema: None,
			}]
		}
		fn call_tool(
			&self,
			name: &str,
			_args: &Value,
		) -> Result<crate::transport::ToolResult, crate::transport::McpError> {
			if name == "add" {
				Ok(crate::transport::ToolResult {
					content: vec![json!({ "type": "text", "text": "ok" })],
					is_error: false,
					structured_content: None,
				})
			} else {
				Err(crate::transport::McpError::Rpc {
					code: -32601,
					message: format!("unknown tool: {name}"),
				})
			}
		}
	}

	fn state(token: Option<&str>) -> AppState<MockServer> {
		AppState {
			server: Arc::new(MockServer),
			token: token.map(Arc::from),
		}
	}

	fn bearer(tok: &str) -> HeaderMap {
		let mut h = HeaderMap::new();
		h.insert(
			axum::http::header::AUTHORIZATION,
			format!("Bearer {tok}").parse().unwrap(),
		);
		h
	}

	async fn body_json(resp: axum::response::Response) -> Value {
		let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
			.await
			.unwrap();
		serde_json::from_slice(&bytes).unwrap()
	}

	#[tokio::test]
	async fn post_tools_list_returns_a_result_listing_the_tool() {
		let body = json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/list" }).to_string();
		let resp = handle_post(State(state(None)), HeaderMap::new(), body)
			.await
			.into_response();
		assert_eq!(resp.status(), StatusCode::OK);
		let v = body_json(resp).await;
		assert_eq!(v["id"], 7, "id is echoed back");
		assert!(
			v.get("error").map(Value::is_null).unwrap_or(true),
			"no error: {v}"
		);
		assert!(v["result"].is_object(), "a result object is present: {v}");
		assert!(
			serde_json::to_string(&v).unwrap().contains("add"),
			"the add tool is listed"
		);
	}

	#[tokio::test]
	async fn post_non_json_body_returns_parse_error() {
		let resp = handle_post(State(state(None)), HeaderMap::new(), "not json".into())
			.await
			.into_response();
		assert_eq!(resp.status(), StatusCode::OK);
		let v = body_json(resp).await;
		assert_eq!(
			v["error"]["code"], -32700,
			"malformed JSON is a parse error"
		);
	}

	#[tokio::test]
	async fn post_with_non_json_content_type_is_rejected() {
		let mut headers = HeaderMap::new();
		headers.insert(CONTENT_TYPE, "text/plain".parse().unwrap());
		let resp = handle_post(State(state(None)), headers, "{}".into())
			.await
			.into_response();
		let v = body_json(resp).await;
		assert_eq!(v["error"]["code"], -32700, "wrong content-type -> -32700");
	}

	fn call_frame() -> String {
		json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
		        "params": { "name": "add", "arguments": {} } })
		.to_string()
	}

	#[tokio::test]
	async fn post_without_a_token_is_rejected_when_one_is_configured() {
		let resp = handle_post(State(state(Some("s3cret"))), HeaderMap::new(), call_frame())
			.await
			.into_response();
		assert_eq!(
			resp.status(),
			StatusCode::UNAUTHORIZED,
			"no Authorization header -> 401"
		);
		let v = body_json(resp).await;
		assert_eq!(v["error"]["code"], -32001);
	}

	#[tokio::test]
	async fn post_with_a_wrong_token_is_rejected() {
		let resp = handle_post(State(state(Some("s3cret"))), bearer("nope"), call_frame())
			.await
			.into_response();
		assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
	}

	#[tokio::test]
	async fn post_with_the_right_token_is_served() {
		let resp = handle_post(State(state(Some("s3cret"))), bearer("s3cret"), call_frame())
			.await
			.into_response();
		assert_eq!(resp.status(), StatusCode::OK, "correct bearer token passes");
	}

	#[tokio::test]
	async fn the_sse_stream_is_gated_too() {
		let unauth = handle_get(State(state(Some("s3cret"))), HeaderMap::new()).await;
		assert_eq!(
			unauth.status(),
			StatusCode::UNAUTHORIZED,
			"GET /mcp must not stream to an unauthenticated caller"
		);
		let ok = handle_get(State(state(Some("s3cret"))), bearer("s3cret")).await;
		assert_eq!(ok.status(), StatusCode::OK);
	}

	#[test]
	fn ct_eq_matches_only_identical_bytes() {
		assert!(ct_eq(b"abc", b"abc"));
		assert!(!ct_eq(b"abc", b"abd"));
		assert!(!ct_eq(b"abc", b"ab"), "a prefix is not a match");
	}
}
