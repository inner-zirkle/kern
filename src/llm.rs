use crate::base::log_throttle::LogThrottle;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::net::Ipv4Addr;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
	#[error("HTTP error: {0}")]
	Http(#[from] reqwest::Error),
	#[error("API error ({status}): {body}")]
	Api { status: u16, body: String },
	#[error("empty embedding response")]
	EmptyEmbedding,
	#[error("empty completion response")]
	EmptyCompletion,
}

pub fn is_transient(err: &LlmError) -> bool {
	match err {
		LlmError::Http(e) => {
			if e.is_timeout() || e.is_connect() {
				return true;
			}
			if let Some(s) = e.status() {
				return s.as_u16() >= 500 || s.as_u16() == 429;
			}
			true
		}
		LlmError::Api { status, .. } => *status >= 500 || *status == 429,
		_ => false,
	}
}

fn should_retry_single(err: &LlmError) -> bool {
	is_transient(err) || matches!(err, LlmError::EmptyEmbedding)
}

const COMPLETE_WARN_SECS: u64 = 60;
static COMPLETE_FAILED: AtomicU64 = AtomicU64::new(0);
static COMPLETE_WARN: LogThrottle = LogThrottle::new(COMPLETE_WARN_SECS);
static LAST_COMPLETE_FAILURE: parking_lot::Mutex<String> = parking_lot::Mutex::new(String::new());

// Completions that failed since this process started. `complete_func` hands its
// caller `""` either way, and downstream reads `""` as "nothing worth keeping",
// so without the count a dead endpoint and a model too weak to answer are the
// same event. Nonzero means the endpoint, not the model.
pub fn complete_failed() -> u64 {
	COMPLETE_FAILED.load(Ordering::Relaxed)
}

// The last failure in words — a timeout, a refused connection and an empty body
// all raise the counter, and only this says which. Empty until one happens.
pub fn last_complete_failure() -> String {
	LAST_COMPLETE_FAILURE.lock().clone()
}

// The recorded reason lands on one line of `kern health`, and `LlmError::Api`
// carries the endpoint's whole body — which is an HTML error page often enough
// to matter. First line, bounded, char-boundary safe.
const REASON_MAX_CHARS: usize = 160;

fn one_line_reason(err: &LlmError) -> String {
	let full = err.to_string();
	let first = full.lines().next().unwrap_or_default().trim();
	match first.char_indices().nth(REASON_MAX_CHARS) {
		Some((i, _)) => format!("{}…", &first[..i]),
		None => first.to_string(),
	}
}

fn record_complete_failure(err: &LlmError) {
	let total = COMPLETE_FAILED.fetch_add(1, Ordering::Relaxed) + 1;
	let transient = is_transient(err);
	// `is_transient` already sorts these cases for the embed leg; reuse its
	// verdict rather than re-deciding what a retryable completion looks like.
	*LAST_COMPLETE_FAILURE.lock() = format!(
		"{}{}",
		if transient {
			"transient: "
		} else {
			"permanent: "
		},
		one_line_reason(err)
	);
	if COMPLETE_WARN.allow() {
		tracing::warn!(
			target: "kern.llm",
			transient,
			total_failed = total,
			error = %err,
			"reason completion failed; the caller sees an empty completion (further failures counted, not logged)"
		);
	}
}

#[derive(Default, Clone)]
pub struct Endpoint {
	pub url: String,
	pub model: String,
	pub key: String,
}

impl Endpoint {
	pub fn new(url: &str, model: &str, key: &str) -> Self {
		Self {
			url: url.to_string(),
			model: model.to_string(),
			key: key.to_string(),
		}
	}
}

#[derive(Clone)]
pub struct Client {
	inner: Arc<Inner>,
}

#[derive(Clone)]
struct Inner {
	reason_url: String,
	reason_model: String,
	reason_headers: HeaderMap,
	reason_native: bool,
	embed_url: String,
	embed_model: String,
	embed_headers: HeaderMap,
	embed_native: bool,
	http: reqwest::Client,
	seed: Option<i64>,
	temperature: Option<f64>,
	num_ctx: Option<u64>,
	embed_num_ctx: u64,
	embed_keep_alive: String,
	reason_keep_alive: String,
	// Per-request ceiling on `complete`. `LLM_TIMEOUT` until `[reason]
	// timeout_secs` says otherwise, and that key defaults to the same number.
	reason_timeout: Duration,
}

impl Client {
	pub fn new(reason: Endpoint, embed: Endpoint) -> Self {
		fn or<'a>(v: &'a str, fallback: &'a str) -> &'a str {
			if v.is_empty() {
				fallback
			} else {
				v
			}
		}
		let embed_url = or(&embed.url, &reason.url);
		let embed_key = or(&embed.key, &reason.key);
		// wants_native reads the pre-normalize URL: normalize strips the `/v1` that marks OpenAI-compat.
		let reason_native = wants_native(&reason.url);
		let embed_native = wants_native(embed_url);
		let normalize = |u: &str| {
			let u = u.trim_end_matches('/');
			u.strip_suffix("/v1").unwrap_or(u).to_string()
		};
		let http = reqwest::Client::builder()
			.timeout(Duration::from_secs(120))
			// Short connect bound so a dead endpoint fails fast (transient -> retry):
			// WSL passthrough to a closed port hangs rather than refusing.
			.connect_timeout(Duration::from_secs(3))
			.build()
			.expect("failed to build HTTP client");
		Self {
			inner: Arc::new(Inner {
				reason_url: normalize(&reason.url),
				reason_model: reason.model.clone(),
				reason_headers: make_headers(&reason.key),
				reason_native,
				embed_url: normalize(embed_url),
				embed_model: embed.model.clone(),
				embed_headers: make_headers(embed_key),
				embed_native,
				http,
				seed: None,
				temperature: None,
				num_ctx: None,
				embed_num_ctx: EMBED_NUM_CTX,
				embed_keep_alive: EMBED_KEEP_ALIVE.to_string(),
				reason_keep_alive: REASON_KEEP_ALIVE.to_string(),
				reason_timeout: LLM_TIMEOUT,
			}),
		}
	}

	pub fn for_eval(mut self, seed: i64) -> Self {
		Arc::make_mut(&mut self.inner).seed = Some(seed);
		self
	}

	pub fn with_temperature(mut self, t: f64) -> Self {
		Arc::make_mut(&mut self.inner).temperature = Some(t);
		self
	}

	// `[reason] timeout_secs`. 0 reads as unset and keeps `LLM_TIMEOUT`, the way
	// an empty url/key falls back rather than taking effect as itself.
	pub fn with_timeout_secs(mut self, secs: u64) -> Self {
		if secs > 0 {
			Arc::make_mut(&mut self.inner).reason_timeout = Duration::from_secs(secs);
		}
		self
	}

	// Ollama-native `complete` only; the OpenAI-compat path has no client-side window.
	// 0 keeps the default — the same convention as `[reason] timeout_secs`.
	pub fn with_num_ctx(mut self, n: u64) -> Self {
		if n > 0 {
			Arc::make_mut(&mut self.inner).num_ctx = Some(n);
		}
		self
	}

	/// `[embed] num_ctx`. 0 keeps the default — the same convention as
	/// `[reason] timeout_secs`. Ollama-native only; ignored on `/v1` (warned at boot).
	pub fn with_embed_num_ctx(mut self, n: u64) -> Self {
		if n > 0 {
			Arc::make_mut(&mut self.inner).embed_num_ctx = n;
		}
		self
	}

	/// `[embed] keep_alive`. Empty keeps the default.
	pub fn with_embed_keep_alive(mut self, ka: &str) -> Self {
		if !ka.is_empty() {
			Arc::make_mut(&mut self.inner).embed_keep_alive = ka.to_string();
		}
		self
	}

	/// `[reason] keep_alive`. Empty keeps the default.
	pub fn with_reason_keep_alive(mut self, ka: &str) -> Self {
		if !ka.is_empty() {
			Arc::make_mut(&mut self.inner).reason_keep_alive = ka.to_string();
		}
		self
	}

	pub fn has_reason(&self) -> bool {
		!self.inner.reason_url.is_empty()
	}

	pub fn new_embed_only(embed_url: &str, embed_model: &str, embed_key: &str) -> Self {
		Self::new(
			Endpoint::default(),
			Endpoint::new(embed_url, embed_model, embed_key),
		)
	}

	pub async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmError> {
		match self.embed_batch(&[text.to_string()]).await {
			Ok(mut vecs) => {
				if vecs.is_empty() {
					return Err(LlmError::EmptyEmbedding);
				}
				Ok(vecs.swap_remove(0))
			}
			Err(e) if should_retry_single(&e) => self.embed_single(text).await,
			Err(e) => Err(e),
		}
	}

	fn embed_body(&self, input: Value) -> Value {
		serde_json::json!({
			"model": self.inner.embed_model,
			"input": input,
			"truncate": true,
			"keep_alive": self.inner.embed_keep_alive,
			"options": { "num_ctx": self.inner.embed_num_ctx },
		})
	}

	async fn post_checked<T: Serialize + ?Sized>(
		&self,
		url: &str,
		headers: &HeaderMap,
		body: &T,
		timeout: Duration,
	) -> Result<reqwest::Response, LlmError> {
		check_status(
			self
				.inner
				.http
				.post(url)
				.headers(headers.clone())
				.json(body)
				.timeout(timeout)
				.send()
				.await?,
		)
		.await
	}

	// `complete`'s post with a transient backoff — a 5xx/429/timeout/connect
	// retries before the caller sees a failure (and `complete_func` records the
	// final one). Permanent errors surface immediately. The embed leg keeps its
	// own `embed_with_retry`; this is the reason leg's mirror.
	async fn post_with_retry<T: Serialize + ?Sized>(
		&self,
		url: &str,
		headers: &HeaderMap,
		body: &T,
		timeout: Duration,
	) -> Result<reqwest::Response, LlmError> {
		let mut last = None;
		for delay_ms in COMPLETE_RETRY_DELAYS_MS.iter() {
			match self.post_checked(url, headers, body, timeout).await {
				Ok(r) => return Ok(r),
				Err(e) if is_transient(&e) => {
					last = Some(e);
					tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
				}
				Err(e) => return Err(e),
			}
		}
		Err(last.expect("retry loop ran at least once"))
	}

	pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LlmError> {
		if self.inner.embed_native {
			let url = format!("{}/api/embed", self.inner.embed_url);
			let body = self.embed_body(serde_json::json!(texts));
			let resp = self
				.post_checked(&url, &self.inner.embed_headers, &body, EMBED_TIMEOUT)
				.await?;
			// /api/embed preserves input order in `embeddings`, so no index sort needed.
			let parsed: NativeEmbedResponse = resp.json().await?;
			if parsed.embeddings.is_empty() {
				return Err(LlmError::EmptyEmbedding);
			}
			return Ok(parsed.embeddings);
		}
		let url = format!("{}/v1/embeddings", self.inner.embed_url);
		let body = serde_json::json!({ "model": self.inner.embed_model, "input": texts });
		let resp = self
			.post_checked(&url, &self.inner.embed_headers, &body, EMBED_TIMEOUT)
			.await?;
		let mut parsed: OpenAiEmbedResponse = resp.json().await?;
		if parsed.data.is_empty() {
			return Err(LlmError::EmptyEmbedding);
		}
		// OpenAI does not guarantee order — sort by index before returning.
		parsed.data.sort_by_key(|i| i.index);
		Ok(parsed.data.into_iter().map(|i| i.embedding).collect())
	}

	async fn embed_single(&self, text: &str) -> Result<Vec<f32>, LlmError> {
		if self.inner.embed_native {
			let url = format!("{}/api/embed", self.inner.embed_url);
			let body = self.embed_body(serde_json::json!(text));
			let resp = self
				.post_checked(&url, &self.inner.embed_headers, &body, EMBED_TIMEOUT)
				.await?;
			let parsed: NativeEmbedResponse = resp.json().await?;
			return parsed
				.embeddings
				.into_iter()
				.next()
				.ok_or(LlmError::EmptyEmbedding);
		}
		let url = format!("{}/v1/embeddings", self.inner.embed_url);
		let body = serde_json::json!({ "model": self.inner.embed_model, "input": text });
		let resp = self
			.post_checked(&url, &self.inner.embed_headers, &body, EMBED_TIMEOUT)
			.await?;
		let parsed: OpenAiEmbedResponse = resp.json().await?;
		parsed
			.data
			.into_iter()
			.next()
			.map(|i| i.embedding)
			.ok_or(LlmError::EmptyEmbedding)
	}

	pub async fn complete(&self, prompt: &str) -> Result<String, LlmError> {
		if self.inner.reason_native {
			let url = format!("{}/api/chat", self.inner.reason_url);
			let mut options =
				serde_json::json!({ "num_ctx": self.inner.num_ctx.unwrap_or(REASON_NUM_CTX) });
			if let Some(s) = self.inner.seed {
				options["seed"] = s.into();
			}
			if let Some(t) = self.inner.temperature {
				options["temperature"] = t.into();
			}
			let body = serde_json::json!({
				"model": self.inner.reason_model,
				"messages": [{"role": "user", "content": prompt}],
				"stream": false,
				"think": false,
				"keep_alive": self.inner.reason_keep_alive,
				"options": options,
			});
			let resp = self
				.post_with_retry(
					&url,
					&self.inner.reason_headers,
					&body,
					self.inner.reason_timeout,
				)
				.await?;
			return resp
				.json::<ChatLine>()
				.await?
				.content
				.map(|t| strip_think(&t))
				.filter(|t| !t.is_empty())
				.ok_or(LlmError::EmptyCompletion);
		}

		let url = format!("{}/v1/chat/completions", self.inner.reason_url);
		let body = ChatRequest {
			model: &self.inner.reason_model,
			messages: vec![ChatMessage {
				role: "user",
				content: prompt,
			}],
			seed: self.inner.seed,
			temperature: self.inner.temperature,
		};
		let resp = self
			.post_with_retry(
				&url,
				&self.inner.reason_headers,
				&body,
				self.inner.reason_timeout,
			)
			.await?;
		let parsed: ChatResponse = resp.json().await?;
		let [c] = parsed.choices;
		let content = strip_think(&c.message.content);
		if content.is_empty() {
			return Err(LlmError::EmptyCompletion);
		}
		Ok(content)
	}

	// The blocking bridge's contract is `String`, and `""` is how every caller
	// reads "nothing came back" — that stays. What does not stay is the silence:
	// the error is counted and named on its way to being discarded, so a hung
	// endpoint, a refused connection and a model too weak to answer stop being
	// the same empty string (ROADMAP item 30).
	pub fn complete_func(&self) -> impl Fn(&str) -> String + Send + Sync + 'static {
		let client = self.clone();
		move |prompt: &str| {
			let client = client.clone();
			let prompt = prompt.to_string();
			match block_on_in_place(client.complete(&prompt)) {
				Some(Ok(text)) => text,
				Some(Err(e)) => {
					record_complete_failure(&e);
					String::new()
				}
				// No runtime to block on: a caller bug, not an endpoint fault, and
				// counting it would read as one.
				None => String::new(),
			}
		}
	}
}

// `block_in_place` makes `block_on` legal on a runtime worker (plain `block_on` panics); `None` outside any runtime.
pub(crate) fn block_on_in_place<F: std::future::Future>(fut: F) -> Option<F::Output> {
	let handle = tokio::runtime::Handle::try_current().ok()?;
	Some(tokio::task::block_in_place(|| handle.block_on(fut)))
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, LlmError> {
	let status = resp.status().as_u16();
	if status >= 400 {
		let body = resp.text().await.unwrap_or_default();
		return Err(LlmError::Api { status, body });
	}
	Ok(resp)
}

fn make_headers(key: &str) -> HeaderMap {
	let mut h = HeaderMap::new();
	h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
	if !key.is_empty() {
		if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
			h.insert(AUTHORIZATION, v);
		}
	}
	h
}

#[derive(Deserialize)]
struct NativeEmbedResponse {
	embeddings: Vec<Vec<f32>>,
}

#[derive(Deserialize)]
struct OpenAiEmbedResponse {
	data: Vec<OpenAiEmbedItem>,
}

#[derive(Deserialize)]
struct OpenAiEmbedItem {
	embedding: Vec<f32>,
	index: usize,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
	model: &'a str,
	messages: Vec<ChatMessage<'a>>,
	#[serde(skip_serializing_if = "Option::is_none")]
	seed: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	temperature: Option<f64>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
	role: &'a str,
	content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
	choices: [ChatChoice; 1],
}

#[derive(Deserialize)]
struct ChatChoice {
	message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
	content: String,
}

// Without a cap Ollama's default-context KV cache can't share a small GPU with the reason model.
pub const EMBED_NUM_CTX: u64 = 2048;

// Keep the embedder resident (`/v1` ignores `keep_alive`); avoids cold reloads between calls.
pub const EMBED_KEEP_ALIVE: &str = "10m";

pub const REASON_NUM_CTX: u64 = 8192;

pub const REASON_KEEP_ALIVE: &str = "2m";

// Overrides the client's 120 s default: slow CPU inference / large RAG prompts / long streams run past it.
// The floor `with_timeout_secs` starts from, and the same number `[reason] timeout_secs` defaults to.
const LLM_TIMEOUT: Duration = Duration::from_secs(crate::config::DEFAULT_REASON_TIMEOUT_SECS);

const EMBED_TIMEOUT: Duration = Duration::from_secs(120);

// Backoff for a transient reason-completion failure (5xx/429/timeout/connect).
// The distill leg is the one LLM latency that matters; a transient blip should
// not re-queue the whole transcript. Three attempts, the embed leg's cadence.
const COMPLETE_RETRY_DELAYS_MS: [u64; 3] = [150, 300, 600];

fn is_local_ollama(url: &str) -> bool {
	url.contains("//localhost") || url.contains("//127.0.0.1") || url.contains(":11434")
}

/// True when the URL's host is local to this machine: loopback, RFC1918,
/// link-local, the `ollama` host, or the `:11434` default-port heuristic
/// `is_local_ollama` already covers. kern's first claim is "local-first, zero
/// egress"; `egress_warnings` uses this so the one setting that voids it is no
/// longer silent at config load. No URL parser dependency — the host is the
/// span between `//` and the first `:`, `/` or end.
pub fn is_local_url(url: &str) -> bool {
	if is_local_ollama(url) {
		return true;
	}
	let host = match host_of(url) {
		Some(h) => h,
		None => return false,
	};
	if host == "localhost" || host == "ollama" {
		return true;
	}
	// Bracketed IPv6 like `[::1]`.
	if let Some(rest) = host.strip_prefix('[') {
		if let Some(v6) = rest.strip_suffix(']') {
			return v6 == "::1";
		}
		return false;
	}
	if let Ok(a) = host.parse::<Ipv4Addr>() {
		return is_local_ipv4(&a.octets());
	}
	false
}

/// True when the URL's host is loopback only — `127.0.0.0/8`, `::1` or
/// `localhost`. Narrower than `is_local_url`: a WSL2 guest reaching a Windows
/// host service must use the RFC1918 gateway IP, not loopback, so a boot
/// warning keys on this.
pub fn is_loopback_url(url: &str) -> bool {
	let host = match host_of(url) {
		Some(h) => h,
		None => return false,
	};
	if host == "localhost" {
		return true;
	}
	if let Some(rest) = host.strip_prefix('[') {
		if let Some(v6) = rest.strip_suffix(']') {
			return v6 == "::1";
		}
		return false;
	}
	if let Ok(a) = host.parse::<Ipv4Addr>() {
		return a.octets()[0] == 127;
	}
	false
}

/// True when this process is running inside WSL (1 or 2). `/proc/sys/kernel/osrelease`
/// carries a Microsoft marker on both; absent or unreadable means not-WSL.
pub fn is_wsl() -> bool {
	std::fs::read_to_string("/proc/sys/kernel/osrelease")
		.map(|k| k.to_ascii_lowercase().contains("microsoft"))
		.unwrap_or(false)
}

fn host_of(url: &str) -> Option<&str> {
	let after = url.split("//").nth(1)?;
	// Bracketed IPv6: host is `[... ]`, the port `:port` follows the `]`.
	let end = if after.starts_with('[') {
		after.find(']').unwrap_or(after.len()) + 1
	} else {
		after.find(['/', ':', '?']).unwrap_or(after.len())
	};
	let h = &after[..end];
	if h.is_empty() {
		None
	} else {
		Some(h)
	}
}

fn is_local_ipv4(o: &[u8; 4]) -> bool {
	o[0] == 127 // loopback 127.0.0.0/8
		|| o[0] == 10 // RFC1918 10.0.0.0/8
		|| (o[0] == 172 && (16..=31).contains(&o[1])) // RFC1918 172.16.0.0/12
		|| (o[0] == 192 && o[1] == 168) // RFC1918 192.168.0.0/16
		|| (o[0] == 169 && o[1] == 254) // link-local 169.254.0.0/16
}

fn wants_native(url: &str) -> bool {
	!url.trim_end_matches('/').ends_with("/v1") && is_local_ollama(url)
}

/// An OpenAI-compatible `/v1` endpoint. Ollama-native knobs (`num_ctx`,
/// `keep_alive`) are not sent on this path — a boot warning uses this to name
/// the knobs a `/v1` config silently ignores.
pub fn is_openai_compat(url: &str) -> bool {
	!wants_native(url) && !url.is_empty()
}

// Terminal chunk may carry both content and `done:true` — emit content before acting on `done`.
#[derive(Debug, PartialEq)]
struct ChatLine {
	content: Option<String>,
	done: bool,
}

impl<'de> Deserialize<'de> for ChatLine {
	fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
		#[derive(Deserialize)]
		struct Wire {
			message: Option<ContentDelta>,
			#[serde(default)]
			done: bool,
		}
		let w = Wire::deserialize(d)?;
		Ok(ChatLine {
			content: w.message.and_then(|m| m.content),
			done: w.done,
		})
	}
}

#[derive(Deserialize)]
struct ContentDelta {
	content: Option<String>,
}

// Reasoning models can leak chain-of-thought into content even with think:false;
// the answer is whatever follows the last closing tag.
fn strip_think(s: &str) -> String {
	let after = match s.rfind("</think>") {
		Some(i) => &s[i + "</think>".len()..],
		None => s,
	};
	let clean = match after.find("<think>") {
		Some(i) => &after[..i],
		None => after,
	};
	clean.trim().to_string()
}

#[cfg(test)]
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
		let (url, _server) = crate::test_support::spawn_http(app).await;
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
		let (url, _server) = crate::test_support::spawn_http(app).await;
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
		let (url, _server) = crate::test_support::spawn_http(app).await;
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

	// The bit-identity claim, checked rather than asserted: the key that replaced
	// the const defaults to the const, and a client built from an unconfigured
	// config posts under the same `Duration` as one that never heard of the key.
	#[test]
	fn the_unconfigured_timeout_is_the_const_it_replaced() {
		let cfg = crate::config::Config::default();
		assert_eq!(
			Duration::from_secs(cfg.reason.timeout_secs),
			LLM_TIMEOUT,
			"an unconfigured kern must post under exactly the old ceiling"
		);
		let plain = Client::new(
			Endpoint::new("http://localhost:11434", "m", ""),
			Endpoint::default(),
		);
		assert_eq!(plain.inner.reason_timeout, LLM_TIMEOUT);
		assert_eq!(
			plain
				.clone()
				.with_timeout_secs(cfg.reason.timeout_secs)
				.inner
				.reason_timeout,
			plain.inner.reason_timeout,
			"applying the default must be a no-op"
		);
		// 0 is "unset", not "give up immediately".
		assert_eq!(
			plain.clone().with_timeout_secs(0).inner.reason_timeout,
			LLM_TIMEOUT
		);
		assert_eq!(
			plain.with_timeout_secs(45).inner.reason_timeout,
			Duration::from_secs(45),
			"a configured ceiling must actually replace it"
		);
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
					let (u, h) = crate::test_support::spawn_http(chat_app(mode)).await;
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
		let (url, _server) = crate::test_support::spawn_http(app).await;
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
		let (url, _server) = crate::test_support::spawn_http(app).await;
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
