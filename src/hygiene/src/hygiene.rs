//! Noise scoring and secret detection. The score is NOT additive: each rule
//! raises the score to at least its own value, so one strong signal is enough
//! and several weak ones cannot compound into a false positive.

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

// ==== [patterns] ====

/// Labelled secret patterns. Labels are what reports carry — the matched value
/// itself is never echoed, so a leaked credential is named, not re-leaked.
const SECRET_PATTERNS: [(&str, &str); 10] = [
	("api_key_prefix", r"(?:sk|pk|rk)-[a-zA-Z0-9]{20,}"),
	("aws_access_key", r"AKIA[0-9A-Z]{16}"),
	("github_token", r"gh[pousr]_[A-Za-z0-9]{36}"),
	("slack_token", r"xox[baprs]-[A-Za-z0-9-]+"),
	("google_api_key", r"AIza[0-9A-Za-z_\-]{35}"),
	(
		"jwt_token",
		r"eyJ[A-Za-z0-9_\-]+\.eyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
	),
	(
		"secret_assignment",
		r#"(?i)(?:password|passwd|pwd|secret|token|api[_-]?key|access[_-]?key)\s*[=:]\s*['"]?[^\s'"<>{}]{8,}"#,
	),
	(
		"private_key_block",
		r"-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP )?PRIVATE KEY-----",
	),
	(
		"connection_string_with_credentials",
		r"(?:postgres|mysql|mongodb|redis)://[^:]+:[^@]+@",
	),
	(
		"env_secret_assignment",
		r"(?im)^\s*(?:DB_PASS|SECRET_KEY|AUTH_TOKEN|API_SECRET)\s*=",
	),
];

/// Curated noise patterns: terminal spam, package-manager output, heartbeats,
/// stack traces, transient status. Multiline (`(?m)`) on purpose — a stack
/// trace pasted mid-thought is still a stack trace.
const NOISE_PATTERNS: [&str; 20] = [
	// Terminal / shell command output
	r"(?m)^\s*(\$|>|#)\s*(pip|npm|npx|yarn|cargo|brew|apt|dnf|pacman)\s",
	r"(?m)^\s*(Collecting|Downloading|Installing|Building|Successfully installed)",
	r"(?m)^\s*Requirement already satisfied",
	r"(?m)^\s*(added|removed|changed)\s+\d+\s+package",
	r"(?im)^\s*(npm warn|npm error|npm notice)",
	r"(?m)^\s*(total\s+\d+|drwx|-\w+-\w+\s)",
	// Heartbeats / cron noise
	r"(?im)^\[?(heartbeat|ping|pong|alive|ok)\]?$",
	r"(?im)^\s*(tick|tock)\s*$",
	r"(?im)^cron\s+(started|completed|skipped|tick)",
	// Stack traces / debug logs
	r"(?m)^Traceback \(most recent call last\):",
	r#"(?m)^\s+File "[^"]+", line \d+"#,
	r"(?m)^\s+(raise|return)\s+\w+Error",
	r"(?m)^(DEBUG|INFO|WARNING|ERROR|CRITICAL)\s+\d{4}-\d{2}-\d{2}",
	r"(?m)^\s*at\s+.*\(.+:\d+:\d+\)",
	r"(?m)^thread '[^']*' panicked at ",
	// Transient status / task progress
	r"(?m)^(Phase|Step|Stage)\s+\d+\s+(done|complete|started|pending)",
	r"(?m)^(PR|Issue|Commit|Merge)\s*#\d+\s+(fixed|done|merged|closed)",
	r"(?m)^\s*(TODO|FIXME|HACK|XXX)\b",
	// Empty / trivial
	r"^\s*$",
	r"(?i)^(ok|done|yes|no|sure|thanks|got it)\.?$",
];

static COMPILED_SECRETS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
	SECRET_PATTERNS
		.iter()
		.map(|(label, pat)| (*label, Regex::new(pat).expect("built-in secret pattern")))
		.collect()
});

static COMPILED_NOISE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
	NOISE_PATTERNS
		.iter()
		.map(|pat| Regex::new(pat).expect("built-in noise pattern"))
		.collect()
});

/// Trivial one-liners that carry no knowledge when they are the whole thought.
const NOISE_KEYWORDS: [&str; 15] = [
	"done",
	"ok",
	"yes",
	"no",
	"sure",
	"thanks",
	"got it",
	"acknowledged",
	"heartbeat",
	"ping",
	"pong",
	"tick",
	"tock",
	"alive",
	"lgtm",
];

/// Value signals that clamp the score DOWN: a thought carrying one of these is
/// what kern exists to keep, so a coarse pattern must not sweep it up. The
/// clamp is skipped when a secret was detected — a secret outranks usefulness.
const VALUE_KEYWORDS: [&str; 13] = [
	"prefer",
	"always",
	"never",
	"decision",
	"decided",
	"because",
	"convention",
	"constraint",
	"architecture",
	"invariant",
	"config",
	"deployment",
	"insight",
];

/// Substring markers of pasted terminal output (checked lowercased).
const TERMINAL_MARKERS: [&str; 9] = [
	"collecting ",
	"downloading ",
	"installing ",
	"requirement already",
	"successfully installed",
	"npm warn",
	"npm error",
	"drwx",
	"-rw-r--r--",
];

/// Compile user-supplied patterns, returning the invalid ones by value so the
/// caller can refuse the config instead of silently dropping a filter the
/// operator believes is active.
pub fn compile_patterns(patterns: &[String]) -> Result<Vec<Regex>, String> {
	let mut out = Vec::with_capacity(patterns.len());
	for p in patterns {
		match Regex::new(p) {
			Ok(re) => out.push(re),
			Err(e) => return Err(format!("invalid hygiene pattern {p:?}: {e}")),
		}
	}
	Ok(out)
}

/// Secret labels present in `content` (labels only, never the matched value).
pub fn detect_secrets(content: &str) -> Vec<&'static str> {
	if content.is_empty() {
		return Vec::new();
	}
	COMPILED_SECRETS
		.iter()
		.filter(|(_, re)| re.is_match(content))
		.map(|(label, _)| *label)
		.collect()
}

// ==== [scoring] ====

/// One scored thought. `score` is 0.0 (valuable) to 1.0 (definitely noise).
#[derive(Debug, Clone, Serialize)]
pub struct NoiseScore {
	pub score: f64,
	pub reasons: Vec<String>,
	pub secrets: Vec<&'static str>,
}

/// Score one thought's text for noise likelihood. `confidence` is the caller's
/// importance signal (kern: the Beta posterior mean) — below 0.2 is itself a
/// weak noise signal, mirroring mnemosyne's `low_importance` rule.
pub fn score_noise(content: &str, confidence: f64) -> NoiseScore {
	let mut reasons: Vec<String> = Vec::new();
	let mut score: f64 = 0.0;

	if content.trim().is_empty() {
		return NoiseScore {
			score: 1.0,
			reasons: vec!["empty_content".into()],
			secrets: Vec::new(),
		};
	}

	let lower = content.trim().to_lowercase();

	if COMPILED_NOISE.iter().any(|re| re.is_match(content)) {
		score = score.max(0.8);
		reasons.push("noise_pattern_match".into());
	}

	let secrets = detect_secrets(content);
	if !secrets.is_empty() {
		score = score.max(0.9);
		reasons.push(format!("secret_detected:{}", secrets.join(",")));
	}

	if lower.len() < 15 && NOISE_KEYWORDS.contains(&lower.as_str()) {
		score = score.max(0.7);
		reasons.push("trivial_keyword".into());
	}

	if TERMINAL_MARKERS.iter().any(|m| lower.contains(m)) {
		score = score.max(0.85);
		reasons.push("terminal_output".into());
	}

	if lower.contains("traceback") || content.contains("  File \"") {
		score = score.max(0.85);
		reasons.push("stack_trace".into());
	}

	if looks_like_dump(content, 30) {
		score = score.max(0.65);
		reasons.push("likely_dump".into());
	}

	if confidence < 0.2 {
		score = score.max(0.5);
		reasons.push("low_importance".into());
	}

	if secrets.is_empty() && VALUE_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
		score = score.min(0.3);
		reasons.push("value_keyword_present".into());
	}

	NoiseScore {
		score,
		reasons,
		secrets,
	}
}

// High line count + low sentence structure = pasted dump, not written knowledge.
fn looks_like_dump(content: &str, min_lines: usize) -> bool {
	let line_count = content.lines().count().max(1);
	if line_count <= min_lines || content.len() <= 1000 {
		return false;
	}
	let sentences = content.matches(". ").count();
	(sentences as f64) < (line_count as f64) * 0.1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestedAction {
	Keep,
	Archive,
	Delete,
	Flag,
}

impl SuggestedAction {
	pub fn as_str(self) -> &'static str {
		match self {
			SuggestedAction::Keep => "keep",
			SuggestedAction::Archive => "archive",
			SuggestedAction::Delete => "delete",
			SuggestedAction::Flag => "flag",
		}
	}
}

/// Secrets are never suggested for deletion — deleting a memory containing a
/// leaked credential destroys the evidence needed to rotate it. They are
/// flagged for a human instead.
pub fn suggest_action(score: f64, has_secrets: bool) -> SuggestedAction {
	if has_secrets {
		SuggestedAction::Flag
	} else if score >= 0.8 {
		SuggestedAction::Delete
	} else if score >= 0.5 {
		SuggestedAction::Archive
	} else {
		SuggestedAction::Keep
	}
}

// ==== [write gate] ====

/// The write-time gate mode. `Off` is the default so the gate's arrival is not
/// a behaviour change; `Warn` logs what strict would refuse; `Strict` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateMode {
	#[default]
	Off,
	Warn,
	Strict,
}

impl GateMode {
	pub fn parse(s: &str) -> Option<Self> {
		match s {
			"off" => Some(GateMode::Off),
			"warn" => Some(GateMode::Warn),
			"strict" => Some(GateMode::Strict),
			_ => None,
		}
	}

	pub fn as_str(self) -> &'static str {
		match self {
			GateMode::Off => "off",
			GateMode::Warn => "warn",
			GateMode::Strict => "strict",
		}
	}
}

/// The gate an ingest job travels with: mode plus the operator's own compiled
/// ignore patterns (built-ins are static and always consulted when on).
#[derive(Debug, Clone, Default)]
pub struct GateConfig {
	pub mode: GateMode,
	pub extra_patterns: Vec<Regex>,
}

/// Why a write was (or would be) refused.
#[derive(Debug, Clone)]
pub struct WriteRejection {
	pub reason: String,
	pub secrets: Vec<&'static str>,
}

/// Classify a write candidate. `None` = allow. Deterministic; stages ordered
/// so the strongest signal names the rejection: empty, secrets, built-in noise
/// patterns, operator patterns, then the structural dump heuristic.
pub fn classify_write(content: &str, extra_patterns: &[Regex]) -> Option<WriteRejection> {
	if content.trim().is_empty() {
		return Some(WriteRejection {
			reason: "empty_content".into(),
			secrets: Vec::new(),
		});
	}
	let secrets = detect_secrets(content);
	if !secrets.is_empty() {
		return Some(WriteRejection {
			reason: format!("secret_detected:{}", secrets.join(",")),
			secrets,
		});
	}
	if COMPILED_NOISE.iter().any(|re| re.is_match(content)) {
		return Some(WriteRejection {
			reason: "noise_pattern_match".into(),
			secrets: Vec::new(),
		});
	}
	if extra_patterns.iter().any(|re| re.is_match(content)) {
		return Some(WriteRejection {
			reason: "ignore_pattern_match".into(),
			secrets: Vec::new(),
		});
	}
	// The write-path dump threshold is looser (50 lines) than the audit's (30):
	// refusing a live write wants more certainty than ranking a stored row.
	if looks_like_dump(content, 50) {
		return Some(WriteRejection {
			reason: "likely_dump_high_linecount_low_structure".into(),
			secrets: Vec::new(),
		});
	}
	None
}

/// What the gate decided. `Warn` carries the classification but the caller
/// must still write — the mode exists so an operator can watch what strict
/// would refuse before turning it on.
#[derive(Debug, Clone)]
pub enum GateDecision {
	Allow,
	Warn(WriteRejection),
	Reject(WriteRejection),
}

/// The gate itself. Pure — the caller owns the emit surface for `Warn`.
pub fn gate_write(content: &str, gate: &GateConfig) -> GateDecision {
	if gate.mode == GateMode::Off {
		return GateDecision::Allow;
	}
	match classify_write(content, &gate.extra_patterns) {
		Some(rej) if gate.mode == GateMode::Strict => GateDecision::Reject(rej),
		Some(rej) => GateDecision::Warn(rej),
		None => GateDecision::Allow,
	}
}

#[cfg(test)]
#[path = "tests/hygiene_test.rs"]
mod hygiene_tests;
