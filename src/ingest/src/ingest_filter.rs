//! Deterministic pre-ingestion noise filter. Rejects known-pattern noise
//! (terminal output, secret tokens, etc.) before they enter the graph.
//! This is a best-effort gate, NOT a security boundary.

use regex::Regex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Result of checking text against the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterResult {
	Pass,
	Reject { reason: String },
}

/// Compiled set of filter patterns.
pub struct WriteFilter {
	patterns: Vec<RejectPattern>,
}

struct RejectPattern {
	reason: String,
	re: Regex,
}

/// Default curated patterns — conservative: false negatives > false positives.
fn default_patterns() -> Vec<(Regex, String)> {
	let raw: &[(&str, &str)] = &[
		// ANSI escape codes (terminal color sequences)
		(r"\x1b\[[0-9;]*[a-zA-Z]", "ANSI escape sequences"),
		// Secret tokens
		(
			r"(?i)\b(?:sk-|pk-)[a-zA-Z0-9]{20,}\b",
			"potential API key/secret token",
		),
		(
			r"-----BEGIN\s+(RSA|EC|OPENSSH|PGP|PRIVATE|ENCRYPTED)\s+PRIVATE\s*KEY-----",
			"private key material",
		),
		(r"ghp_[a-zA-Z0-9]{36}", "GitHub personal access token"),
		(r"gho_[a-zA-Z0-9]{36}", "GitHub OAuth access token"),
		// Git noise
		(r"^On branch\s+\S+", "git status noise"),
		(r"^nothing to commit", "git status noise"),
		(r"^Already up['\u2019]?date", "git pull noise"),
		(r"^Your branch is up to date with", "git status noise"),
		// Directory listings
		(r"^total \d+$", "directory listing (ls header)"),
		(
			r"^drwxr[-x][r-x][r-x].*\d+ .* \d+:",
			"directory listing (ls -l dir)",
		),
		(
			r"^-rw[-r][-w][-x].*\d+ .* \d+:",
			"directory listing (ls -l file)",
		),
		// Heartbeat / keepalive
		(r"(?i)^(ping|pong|heartbeat|keepalive)$", "heartbeat/ping"),
		// Shell prompt noise (leading $ or % at start)
		(r"^\$\s", "shell prompt noise"),
		(r"^%\s", "shell prompt noise"),
	];
	raw.iter()
		.map(|&(raw_pat, desc)| {
			(Regex::new(raw_pat).expect("invalid filter pattern"), desc.to_string())
		})
		.collect()
}

impl WriteFilter {
	/// Create with custom patterns. Empty = use defaults.
	pub fn new(custom_patterns: &[String]) -> Self {
		let patterns: Vec<RejectPattern> = if custom_patterns.is_empty() {
			default_patterns()
				.into_iter()
				.map(|(re, reason)| RejectPattern { reason, re })
				.collect()
		} else {
			custom_patterns
				.iter()
				.map(|pat| {
					let re = Regex::new(pat).unwrap_or_else(|e| {
						tracing::warn!(
							target: "kern.filter",
							pattern = %pat,
							error = %e,
							"invalid filter pattern, skipping"
						);
						// Use a pattern that never matches
						Regex::new("a^").unwrap()
					});
					RejectPattern {
						reason: pat.clone(),
						re,
					}
				})
				.collect()
		};
		Self { patterns }
	}

	/// Check text against filter. Returns `Pass` or `Reject` with reason.
	pub fn check(&self, text: &str) -> FilterResult {
		for p in &self.patterns {
			if p.re.is_match(text) {
				return FilterResult::Reject {
					reason: p.reason.to_string(),
				};
			}
		}
		FilterResult::Pass
	}
}

// Global counter of filtered-out submissions.
static FILTER_REJECTED: AtomicU64 = AtomicU64::new(0);

/// Increment the global rejected counter.
pub fn increment_filter_rejected() {
	FILTER_REJECTED.fetch_add(1, Ordering::Relaxed);
}

/// Read the global rejected counter.
pub fn filter_rejected_count() -> u64 {
	FILTER_REJECTED.load(Ordering::Relaxed)
}
