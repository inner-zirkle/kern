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
			util::DISTILL_LADDER.step_down("LLM distill returned None");
			return None;
		}
		match parse_claims(&raw, extra_kinds) {
			Some(claims) => all.extend(claims),
			// A batch that returns no parseable array is a format failure for
			// the whole delta — retry, never archive a partially-distilled
			// conversation that silently dropped every later batch.
			None => {
				util::DISTILL_LADDER.step_down("LLM distill parse failure");
				return None;
			}
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
mod tests {
	fn now() -> std::time::SystemTime {
		std::time::UNIX_EPOCH
	}
	use super::*;

	fn stub(json: &'static str) -> impl Fn(&str) -> String {
		move |_q: &str| json.to_string()
	}

	#[test]
	fn extracts_claims_and_maps_kind() {
		let llm = stub(
			r#"[{"text":"User prefers tabs","kind":"preference"},{"text":"kern owns the graph","kind":"code-fact"}]"#,
		);
		let claims = distill("some conversation", &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 2);
		assert_eq!(claims[0].text, "User prefers tabs");
		assert_eq!(claims[0].kind, "preference");
		assert_eq!(claims[1].kind, "code-fact");
	}

	#[test]
	fn procedural_kind_maps_through() {
		let llm = stub(r#"[{"text":"Always run cargo test before committing","kind":"procedural"}]"#);
		let claims = distill("c", &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert_eq!(claims[0].kind, "procedural");
		assert!(DEFAULT_KINDS.contains(&"procedural"));
	}

	#[test]
	fn unknown_kind_falls_back_to_fact() {
		let llm = stub(r#"[{"text":"x","kind":"banana"}]"#);
		let claims = distill("c", &[], &llm, now()).expect("some");
		assert_eq!(claims[0].kind, "fact");
	}

	#[test]
	fn parse_claims_records_turn_provenance() {
		let llm = stub(r#"[{"text":"the key is in vault X","kind":"fact","turns":[1,3]}]"#);
		let claims = distill("turn one\n\nturn two\n\nturn three", &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert_eq!(claims[0].turns, vec![1, 3], "cited turn numbers round-trip");
	}

	#[test]
	fn turns_absent_or_malformed_leaves_empty() {
		let no_turns = stub(r#"[{"text":"x","kind":"fact"}]"#);
		assert!(distill("c", &[], &no_turns, now()).expect("some")[0]
			.turns
			.is_empty());
		// floats accepted, zeros/negatives dropped — degrades to empty, never panics
		let messy = stub(r#"[{"text":"y","kind":"fact","turns":[2.0,0,"oops"]}]"#);
		assert_eq!(
			distill("c", &[], &messy, now()).expect("some")[0].turns,
			vec![2]
		);
	}

	#[test]
	fn split_turns_breaks_on_blank_lines() {
		assert_eq!(split_turns("a\n\nb\n\nc"), vec!["a", "b", "c"]);
		assert_eq!(split_turns("one block"), vec!["one block"]);
		assert_eq!(
			split_turns("\r\na\r\n\r\nb"),
			vec!["a", "b"],
			"CRLF normalized"
		);
		assert!(split_turns("\n\n\n").is_empty());
	}

	#[test]
	fn registered_kind_is_accepted_and_offered_to_the_llm() {
		let seen = std::sync::Mutex::new(String::new());
		let llm = |p: &str| {
			*seen.lock().unwrap() = p.to_string();
			r#"[{"text":"finding X","kind":"audit-finding"}]"#.to_string()
		};
		let extra = vec!["audit-finding".to_string()];
		let claims = distill("c", &extra, &llm, now()).expect("some");
		assert_eq!(claims[0].kind, "audit-finding");
		assert!(
			seen.lock().unwrap().contains("audit-finding"),
			"registered kind is listed in the prompt"
		);
	}

	#[test]
	fn kind_list_dedups_registered_defaults() {
		let extra = vec!["fact".to_string(), "custom".to_string()];
		let list = kind_list(&extra);
		assert_eq!(list.matches("fact").count(), 2, "fact + code-fact only");
		assert!(list.ends_with(", custom"));
	}

	#[test]
	fn prose_reply_signals_retry_not_archive() {
		let llm = stub("I could not find anything useful, sorry!");
		assert!(
			distill("c", &[], &llm, now()).is_none(),
			"a prose reply with no JSON array is a format failure — retry, never archive"
		);
	}

	#[test]
	fn prose_reply_carrying_knowledge_is_not_lost() {
		// A weak model that answers in prose instead of JSON must not cause the
		// delta to be archived having stored nothing.
		let llm = stub("The user prefers tabs, and they decided to deploy on Fridays.");
		assert!(
			distill("a real conversation", &[], &llm, now()).is_none(),
			"non-JSON reply carrying real knowledge signals retry, so nothing is silently lost"
		);
	}

	#[test]
	fn empty_conversation_skips_llm() {
		let llm = stub(r#"[{"text":"should not appear","kind":"fact"}]"#);
		assert!(distill("   \n  ", &[], &llm, now())
			.expect("some")
			.is_empty());
	}

	#[test]
	fn empty_llm_response_signals_retry() {
		let llm = stub("");
		assert!(distill("a real conversation worth keeping", &[], &llm, now()).is_none());
	}

	#[test]
	fn whitespace_llm_response_signals_retry() {
		let llm = stub("   \n\t ");
		assert!(distill("a real conversation", &[], &llm, now()).is_none());
	}

	#[test]
	fn genuine_empty_array_is_some_empty() {
		let llm = stub("[]");
		assert_eq!(
			distill("a real conversation", &[], &llm, now()),
			Some(Vec::new())
		);
	}

	#[test]
	fn tolerates_prose_around_json() {
		let llm = stub("Here you go:\n[{\"text\":\"a\",\"kind\":\"fact\"}]\nHope that helps");
		let claims = distill("c", &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert_eq!(claims[0].text, "a");
	}

	#[test]
	fn valid_from_hint_is_parsed_when_present_and_ignored_when_garbage() {
		let good = stub(
			r#"[{"text":"we moved to spaces","kind":"decision","valid_from":"2026-03-01T00:00:00Z"}]"#,
		);
		let claims = distill("c", &[], &good, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert!(
			claims[0].valid_from.is_some(),
			"a valid ISO valid_from is parsed"
		);

		let garbage = stub(r#"[{"text":"x","kind":"fact","valid_from":"since March"}]"#);
		assert_eq!(
			distill("c", &[], &garbage, now()).expect("some")[0].valid_from,
			None,
			"an unparseable valid_from is ignored, not fatal"
		);

		let absent = stub(r#"[{"text":"y","kind":"fact"}]"#);
		assert_eq!(
			distill("c", &[], &absent, now()).expect("some")[0].valid_from,
			None
		);
	}

	#[test]
	fn absent_kind_falls_back_to_fact() {
		let llm = stub(r#"[{"text":"x"}]"#);
		let claims = distill("c", &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert_eq!(claims[0].kind, "fact");
	}

	#[test]
	fn empty_or_missing_text_is_skipped() {
		let llm = stub(r#"[{"text":"","kind":"fact"},{"kind":"fact"},{"text":"keep","kind":"fact"}]"#);
		let claims = distill("c", &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert_eq!(claims[0].text, "keep");
	}

	#[test]
	fn single_nested_array_is_unwrapped() {
		let llm = stub(r#"[[{"text":"a","kind":"fact"}]]"#);
		let claims = distill("c", &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert_eq!(claims[0].text, "a");
	}

	#[test]
	fn multiple_sibling_arrays_signal_retry() {
		let two_siblings = stub(r#"[{"text":"a","kind":"fact"}] [{"text":"b","kind":"fact"}]"#);
		assert!(
			distill("c", &[], &two_siblings, now()).is_none(),
			"sibling arrays span to invalid JSON — a format failure, so retry not archive",
		);
	}

	#[test]
	fn len2_array_of_arrays_parses_to_empty() {
		// Valid JSON array, just the wrong shape: it parsed, so it archives as a
		// genuine no-claims result rather than retrying forever.
		let array_of_arrays = stub(r#"[[{"text":"a","kind":"fact"}],[{"text":"b","kind":"fact"}]]"#);
		assert!(
			distill("c", &[], &array_of_arrays, now())
				.expect("some")
				.is_empty(),
			"a len-2 array-of-arrays is neither unwrapped nor merged",
		);
	}
	#[test]
	fn distill_prompt_injects_current_date_for_relative_resolution() {
		let captured = std::sync::Mutex::new(String::new());
		let llm = |p: &str| {
			*captured.lock().unwrap() = p.to_string();
			r#"[{"text":"x","kind":"fact"}]"#.to_string()
		};
		let now = util::parse_rfc3339("2026-07-22T00:00:00").unwrap();
		let _ = distill("some conversation about last Tuesday", &[], &llm, now);
		let prompt = captured.into_inner().unwrap();
		assert!(
			prompt.contains("2026-07-22"),
			"prompt must name today for relative-date resolution: {prompt}"
		);
	}

	#[test]
	fn distill_short_conversation_is_one_call() {
		// turns.len() <= DISTILL_CHUNK_TURNS -> one llm call (common case,
		// bit-identical to the pre-chunking path).
		let calls = std::sync::atomic::AtomicUsize::new(0);
		let llm = |_p: &str| {
			calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
			r#"[{"text":"x","kind":"fact"}]"#.to_string()
		};
		let conv = (0..DISTILL_CHUNK_TURNS)
			.map(|i| format!("turn {i}"))
			.collect::<Vec<_>>()
			.join("\n\n");
		let claims = distill(&conv, &[], &llm, now()).expect("some");
		assert_eq!(claims.len(), 1);
		assert_eq!(
			calls.load(std::sync::atomic::Ordering::SeqCst),
			1,
			"at exactly the batch boundary there is one call"
		);
	}

	#[test]
	fn distill_chunks_long_conversation_turn_batched() {
		// N > DISTILL_CHUNK_TURNS -> ceil(N/batch) calls, one claim per batch,
		// no batch silently dropped. The stub tags each claim with its batch.
		let n = DISTILL_CHUNK_TURNS + 5;
		let calls = std::sync::atomic::AtomicUsize::new(0);
		let llm = |_p: &str| {
			let b = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
			format!(r#"[{{"text":"batch {b}","kind":"fact"}}]"#)
		};
		let conv = (0..n)
			.map(|i| format!("turn {i}"))
			.collect::<Vec<_>>()
			.join("\n\n");
		let claims = distill(&conv, &[], &llm, now()).expect("some");
		let expected = n.div_ceil(DISTILL_CHUNK_TURNS);
		assert_eq!(
			calls.load(std::sync::atomic::Ordering::SeqCst),
			expected,
			"ceil(N/batch) llm calls"
		);
		assert_eq!(claims.len(), expected, "one claim per batch, none dropped");
		// every batch index present
		for b in 0..expected {
			assert!(
				claims.iter().any(|c| c.text == format!("batch {b}")),
				"batch {b} claim present"
			);
		}
	}

	#[test]
	fn distill_chunk_markers_carry_global_turn_index() {
		// a claim citing the first turn of batch 2 resolves to the global turn
		// number, not a per-batch 0 — markers are offset by the batch start.
		let n = DISTILL_CHUNK_TURNS + 1;
		let captured = std::sync::Mutex::new(String::new());
		let llm = |p: &str| {
			if p.contains(&format!("[{}] ", DISTILL_CHUNK_TURNS)) {
				*captured.lock().unwrap() = p.to_string();
			}
			r#"[{"text":"x","kind":"fact"}]"#.to_string()
		};
		let conv = (0..n)
			.map(|i| format!("turn {i}"))
			.collect::<Vec<_>>()
			.join("\n\n");
		let _ = distill(&conv, &[], &llm, now());
		let prompt = captured.into_inner().unwrap();
		assert!(
			prompt.contains(&format!(
				"[{}] turn {}",
				DISTILL_CHUNK_TURNS, DISTILL_CHUNK_TURNS
			)),
			"batch 2's first marker is the global turn index, not 0: {prompt}"
		);
	}

	#[test]
	fn distill_batch_format_failure_retries_whole_delta() {
		// one batch returns prose (no JSON array) -> None, even if an earlier
		// batch parsed. A partially-distilled conversation must not archive.
		let n = DISTILL_CHUNK_TURNS + 1;
		let llm = |p: &str| {
			if p.contains(&format!("[{}] ", DISTILL_CHUNK_TURNS)) {
				"prose reply with no array".to_string()
			} else {
				r#"[{"text":"x","kind":"fact"}]"#.to_string()
			}
		};
		let conv = (0..n)
			.map(|i| format!("turn {i}"))
			.collect::<Vec<_>>()
			.join("\n\n");
		assert!(
			distill(&conv, &[], &llm, now()).is_none(),
			"a batch format failure retries the whole delta, never archives partial"
		);
	}
}
