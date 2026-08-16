//! Transcript distillation: chunk a conversation by turns, prompt the LLM for
//! self-contained claims (kind-labelled, relative dates resolved against a
//! known today), and parse its output defensively — a malformed reply drops
//! the chunk, never the pass.

use base::base_constants::DISTILL_CHUNK_TURNS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
	pub text: String,
	pub kind: String,
	pub valid_from: Option<std::time::SystemTime>,
	// 1-based turn numbers in the transcript the claim was drawn from, when the
	// distill LLM cited them. Empty = uncited (the graph still carries the claim;
	// the section carrier stays empty, matching the pre-provenance baseline).
	pub turns: Vec<usize>,
}

// Split a transcript into turns on blank-line boundaries — the same unit the
// direct path (paragraph_split) and the LoCoMo harness (turns joined by "\n\n")
// use, so a 1-based turn number here maps to the same turn the caller indexed.
fn split_turns(conversation: &str) -> Vec<String> {
	conversation
		.replace("\r\n", "\n")
		.split("\n\n")
		.map(str::trim)
		.filter(|t| !t.is_empty())
		.map(str::to_string)
		.collect()
}

// The built-in claim kinds; registered kinds (root.claim_kinds) extend this set.
pub const DEFAULT_KINDS: [&str; 7] = [
	"preference",
	"decision",
	"project",
	"fact",
	"code-fact",
	"reference",
	"procedural",
];

fn kind_list(extra_kinds: &[String]) -> String {
	let mut kinds: Vec<&str> = DEFAULT_KINDS.to_vec();
	for k in extra_kinds {
		if !kinds.contains(&k.as_str()) {
			kinds.push(k);
		}
	}
	kinds.join(", ")
}

/// `Some([])` = the LLM emitted a well-formed JSON array holding nothing worth
/// keeping (archive). `None` = no usable output — an empty response OR a prose
/// reply with no parseable JSON array (a weak model ignoring the format is a soft
/// outage, not a genuine "nothing"): the caller must retry, never archive, so the
/// delta is not silently lost.
pub fn distill(
	conversation: &str,
	extra_kinds: &[String],
	llm: &dyn Fn(&str) -> String,
	now: std::time::SystemTime,
) -> Option<Vec<Claim>> {
	if conversation.trim().is_empty() {
		return Some(Vec::new());
	}
	let kinds = kind_list(extra_kinds);
	// Inline 1-based turn markers so the model can cite which turns a claim is
	// drawn from; the citation populates Source::Session.section at ingest.
	let turns = split_turns(conversation);
	let today = util::date_string(now);
	// Turn-batched chunking: a conversation longer than DISTILL_CHUNK_TURNS is
	// split into batches of that many turns, each distilled through its own
	// prompt, so a long delta stops truncating past the model context window
	// with no signal (item 49 chunking half). Markers carry the global turn
	// index so a citation in batch 2 maps to the right transcript turn. The
	// common case (turns.len() <= DISTILL_CHUNK_TURNS) is one batch, bit-
	// identical to the pre-chunking single call.
	let mut all: Vec<Claim> = Vec::new();
	for (batch_idx, chunk) in turns.chunks(DISTILL_CHUNK_TURNS).enumerate() {
		let start = batch_idx * DISTILL_CHUNK_TURNS;
		let marked: String = chunk
			.iter()
			.enumerate()
			.map(|(i, t)| format!("[{}] {t}", start + i))
			.collect::<Vec<_>>()
			.join("\n\n");
		let prompt = format!(
			"Extract durable, reusable knowledge from this conversation between a \
user and an AI coding assistant. The transcript below is marked with 1-based \
turn numbers in [brackets]. Output ONLY a JSON array. Each element must be \
{{\"text\": \"<one self-contained statement>\", \"kind\": \"<one of: {kinds}>\"}}. Optionally add \
\"valid_from\": \"<ISO8601 date>\" ONLY when the statement itself says when it \
became true (e.g. \"since March 2026\", \"as of v2\"); resolve relative date \
phrases (\"last Tuesday\", \"yesterday\", \"two weeks ago\") to the absolute ISO8601 \
date against today, which is {today}. Omit valid_from when the statement \
carries no date. \
Optionally add \"turns\": [<1-based turn numbers the claim is drawn from, as marked>] \
when the claim is grounded in specific turns; omit it when it spans the whole \
transcript or is uncertain. \
Include only knowledge worth \
remembering across future sessions: user preferences, decisions and their \
rationale, ongoing project state, durable facts, structural code facts, \
external references, and procedural knowledge (learned workflows, rules, and \
conventions — how we do X, not just what is true). \
Consolidate aggressively: emit ONE claim per distinct fact. Do NOT output \
multiple claims that restate the same idea, and do NOT output sentence \
fragments — each claim must be a complete, standalone statement that captures \
the fact in full. Prefer the single most complete phrasing over several \
partial ones. \
Skip greetings, acknowledgements, one-off task mechanics, and anything \
ephemeral. If nothing is worth keeping, output []. Do not wrap the array in \
markdown.\n\nCONVERSATION:\n{marked}\n"
		);
		let raw = llm(&prompt);
		if raw.trim().is_empty() {
			return None;
		}
		match parse_claims(&raw, extra_kinds) {
			Some(claims) => all.extend(claims),
			// A batch that returns no parseable array is a format failure for
			// the whole delta — retry, never archive a partially-distilled
			// conversation that silently dropped every later batch.
			None => return None,
		}
	}
	Some(all)
}

/// `None` = the reply held no parseable JSON array (prose or malformed span) — a
/// format failure the caller must retry, not archive. `Some(vec)` = an array
/// parsed; the vec may be empty once empty-text items are filtered, which is a
/// genuine "nothing worth keeping".
pub(crate) fn parse_claims(raw: &str, extra_kinds: &[String]) -> Option<Vec<Claim>> {
	let (start, end) = match (raw.find('['), raw.rfind(']')) {
		(Some(s), Some(e)) if e > s => (s, e),
		_ => return None,
	};
	let mut items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..=end]) {
		Ok(v) => v,
		Err(e) => {
			tracing::debug!(target: "kern.distill", error = %e, "claim JSON parse failed");
			return None;
		}
	};
	// Unwrap a lone `[[...]]` wrapper (LLM quirk).
	if items.len() == 1 {
		if let Some(inner) = items[0].as_array_mut() {
			items = std::mem::take(inner);
		}
	}
	let mut out = Vec::new();
	for it in items {
		let text = it
			.get("text")
			.and_then(|v| v.as_str())
			.unwrap_or("")
			.trim()
			.to_string();
		if text.is_empty() {
			continue;
		}
		let kind_raw = it
			.get("kind")
			.and_then(|v| v.as_str())
			.unwrap_or("fact")
			.trim();
		let kind = if DEFAULT_KINDS.contains(&kind_raw) || extra_kinds.iter().any(|k| k == kind_raw) {
			kind_raw.to_string()
		} else {
			"fact".to_string()
		};
		let valid_from = it
			.get("valid_from")
			.and_then(|v| v.as_str())
			.map(str::trim)
			.filter(|s| !s.is_empty())
			.and_then(|s| util::parse_rfc3339(s).ok());
		// 1-based turn citations from the marked transcript; non-integer or < 1
		// entries are dropped, so a malformed `turns` degrades to empty (uncited),
		// never to a panic or a wrong turn.
		let turns: Vec<usize> = it
			.get("turns")
			.and_then(|v| v.as_array())
			.map(|a| {
				a.iter()
					.filter_map(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64)))
					.filter(|n| *n >= 1)
					.map(|n| n as usize)
					.collect()
			})
			.unwrap_or_default();
		out.push(Claim {
			text,
			kind,
			valid_from,
			turns,
		});
	}
	Some(out)
}

#[cfg(test)]
#[path = "tests/ingest_distill_test.rs"]
mod ingest_distill_tests;
