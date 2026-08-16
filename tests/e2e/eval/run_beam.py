"""BEAM (Mohammadta/BEAM), end-to-end.

Per conversation: a fresh kern project ingests every chat message verbatim
(direct path, no LLM), then every probing question runs `kern query`, the
retrieved hits — resolved back to their full stored text via `kern get` —
become the only context an answer LLM sees, and a judge LLM scores that
answer against the dataset's per-question rubric (the BEAM nugget protocol).
Scores are reported per ability (IE, MR, TR, ABS, CR, KU, EO, IF, PF, SUM)
and overall, per scale.

`--mode retrieval` drops both LLMs and reports a content-token coverage
proxy (ideal-answer tokens found in the retrieved context) — a plumbing and
ablation number, NOT comparable to LLM-judged BEAM scores.

Integrity rules, inherited from the mnemosyne BEAM postmortem (its
docs/benchmarking.md; every one of these silently inflated scores there):
- no harness-side oracles: this runner never answers, hints at, or computes
  any question itself — no timeline extraction from raw messages, no
  contradiction detection injected into prompts, no regex fact index built
  at ingest and consulted at answer time;
- no recency anchoring: raw conversation messages are never prepended to the
  answer prompt — the only context is what `kern query` returned;
- every piece of context flows through kern's real query path (`kern get`
  merely undoes the CLI's 120-char print truncation on a returned hit — it
  adds no material retrieval did not select);
- the judge model is configured and reported separately from the answer
  model, and an unparseable judge reply scores 0 and is counted — never
  silently patched over with a similarity heuristic.
"""

import ast
import json
import re
import tempfile
import time
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path

import score
from common import (
	DATA_DIR,
	argparser,
	base_report,
	build_kern,
	ingest_session,
	make_project,
	open_endpoints,
	parse_args,
	resolve_full_text,
	sanitize,
	write_report,
)
from ranking import hits

# The ten BEAM memory abilities, in the paper's reporting order.
ABILITIES = ["IE", "MR", "TR", "ABS", "CR", "KU", "EO", "IF", "PF", "SUM"]

# Dataset ability names (probing_questions keys) -> abbreviations.
ABILITY_MAP = {
	"information_extraction": "IE",
	"multi_session_reasoning": "MR",
	"temporal_reasoning": "TR",
	"abstention": "ABS",
	"contradiction_resolution": "CR",
	"knowledge_update": "KU",
	"event_ordering": "EO",
	"instruction_following": "IF",
	"preference_following": "PF",
	"summarization": "SUM",
}

# Splits shipped in Mohammadta/BEAM. The 10M tier lives in a separate
# streaming-scale repo (Mohammadta/BEAM-10M) and is out of scope here.
SCALES = ("100K", "500K", "1M")

# The ideal answer's key varies per ability (ABS uses ideal_response, SUM
# ideal_summary, …); IF and PF ship only a rubric, which is fine — judging
# is rubric-based either way.
IDEAL_KEYS = ("ideal_answer", "ideal_response", "answer", "ideal_summary")

MAX_CONTEXT_CHARS = 12000
MAX_TURNS_PER_FILE = 250

# One neutral prompt for every ability. mnemosyne's harness carried seven
# ability-specific specialist prompts (EO/KU JSON ultimatums, a CR detector,
# an ABS gate, an anti-abstention clause in the generic prompt…) tuned
# against its judge's format quirks; that surface was part of what its
# postmortem had to unwind. kern answers every question the same way and
# lets retrieval quality carry the score.
ANSWER_SYSTEM = """You are answering questions about a past conversation.
You are given MEMORY CONTEXT retrieved from a memory system, then a question.
Answer using ONLY the memory context.
- Be specific: quote exact dates, numbers, names and versions when present.
- If the context contains contradictory statements about the asked topic,
  say so explicitly and cite both sides.
- For ordering questions, produce a numbered chronological list.
- If the context does not contain the information needed, reply exactly:
  "This information is not present in the conversation."
"""

JUDGE_SYSTEM = """You are an expert evaluator for a memory benchmark.
You get a question, a list of RUBRIC ITEMS (expected facts), and an ANSWER.
For EACH rubric item, score whether the answer contains equivalent information:
1.0 = present and substantially correct; 0.5 = partially correct;
0.0 = missing or wrong.
Return ONLY this JSON: {"scores": [one number per rubric item]}"""

STOP = frozenset(
	"the a an and or of to in on at for with is are was were be been i you my "
	"your me we they it its this that these have has had do does did not no yes "
	"how what when where which who why many much should would could can will "
	"about between across from into any all some there their".split()
)


def load_conversations(path, counts):
	"""beam_<scale>.json (see datasets.py) -> conversations with flat questions.

	`probing_questions` is a Python-literal string in the upstream dataset;
	`chat` is a list of session blocks, each a list of message dicts.
	"""
	convs = []
	for row in json.loads(path.read_text()):
		probing = ast.literal_eval(row["probing_questions"])
		questions = []
		for ability, qs in probing.items():
			ab = ABILITY_MAP.get(ability)
			if ab is None:
				counts["skipped_unknown_ability"] += len(qs)
				continue
			for q in qs:
				text = (q.get("question") or "").strip()
				rubric = [str(r) for r in q.get("rubric") or []]
				if not text or not rubric:
					counts["skipped_no_rubric"] += 1
					continue
				ideal = next((str(q[key]) for key in IDEAL_KEYS if q.get(key)), "")
				questions.append(
					{"ability": ab, "question": text, "ideal": ideal, "rubric": rubric}
				)
		convs.append(
			{
				"id": str(row["conversation_id"]),
				"sessions": row["chat"],
				"questions": questions,
			}
		)
	return convs


def turn_text(msg):
	"""One chat message -> one stored Document, time anchor included.

	The anchor is part of the conversation record itself (every BEAM message
	carries one), so baking it into the stored text keeps temporal evidence on
	the retrieval path — the harness never re-reads it at answer time.
	"""
	text = (msg.get("content") or "").strip()
	if not text:
		return ""
	anchor = (msg.get("time_anchor") or "").strip()
	prefix = f"[{anchor}] " if anchor else ""
	return sanitize(f"{prefix}{msg.get('role', 'user')}: {text}")


def ingest_conversation(project, sessions, tmp, counts):
	for si, session in enumerate(sessions):
		texts = [t for t in (turn_text(m) for m in session) if t]
		for ci in range(0, len(texts), MAX_TURNS_PER_FILE):
			chunk = texts[ci : ci + MAX_TURNS_PER_FILE]
			status, _ = ingest_session(project, chunk, tmp, f"s{si}_{ci}")
			counts[f"ingest_{status}"] += 1


def retrieve_context(project, question, cache, k):
	"""The only source of answer context: kern's own `kern query` path.

	Hits print truncated at 120 chars; `kern get` on the hit's id restores the
	full stored text — undoing presentation truncation, adding nothing.
	Integrity: no raw conversation messages, no recency window, no side
	index — mnemosyne postmortem bugs 1–3.
	"""
	t0 = time.monotonic()
	stdout, _ = project.run("query", question)
	secs = time.monotonic() - t0
	lines = []
	total = 0
	for h in hits(stdout)[:k]:
		if h.short_id not in cache:
			cache[h.short_id] = resolve_full_text(project, h.short_id) or h.text
		text = cache[h.short_id]
		if lines and total + len(text) > MAX_CONTEXT_CHARS:
			break
		lines.append(text)
		total += len(text)
	return lines, secs


def chat(url, model, system, user, timeout=600, attempts=3):
	"""One chat completion against the native Ollama-style /api/chat."""
	req = urllib.request.Request(
		f"{url.rstrip('/')}/api/chat",
		data=json.dumps(
			{
				"model": model,
				"messages": [
					{"role": "system", "content": system},
					{"role": "user", "content": user},
				],
				"stream": False,
			}
		).encode(),
		headers={"Content-Type": "application/json"},
	)
	last = None
	for attempt in range(1, attempts + 1):
		try:
			with urllib.request.urlopen(req, timeout=timeout) as resp:
				body = json.loads(resp.read())
			return (body.get("message") or {}).get("content") or ""
		except (urllib.error.URLError, TimeoutError, OSError, json.JSONDecodeError) as e:
			last = e
			if attempt < attempts:
				time.sleep(5 * attempt)
	raise RuntimeError(f"chat endpoint {url} failed after {attempts} attempts: {last}")


def answer_question(endpoint, question, context_lines):
	url, model = endpoint
	context = "\n".join(f"{i + 1}. {t}" for i, t in enumerate(context_lines))
	user = f"MEMORY CONTEXT:\n{context}\n\nQUESTION: {question}\n\nAnswer:"
	return chat(url, model, ANSWER_SYSTEM, user)


def judge_answer(endpoint, question, rubric, answer):
	"""(score, parsed_ok). Score is the mean of per-rubric-item scores.

	The mean is computed here, each item clamped to [0, 1] — a judge-side
	"overall" field is never trusted (its arithmetic is unauditable and a
	hallucinated 0.9 would go straight into the report).
	"""
	url, model = endpoint
	rubric_text = "\n".join(f"{i + 1}. {item}" for i, item in enumerate(rubric))
	user = (
		f"QUESTION: {question}\n\nRUBRIC ITEMS:\n{rubric_text}\n\n"
		f"ANSWER: {answer}\n\n"
		"Score each rubric item. Return only the JSON object."
	)
	reply = chat(url, model, JUDGE_SYSTEM, user)
	start, end = reply.find("{"), reply.rfind("}") + 1
	if 0 <= start < end:
		try:
			scores = json.loads(reply[start:end]).get("scores")
			if isinstance(scores, list) and scores:
				vals = [min(1.0, max(0.0, float(s))) for s in scores]
				return sum(vals) / len(vals), True
		except (ValueError, TypeError):
			pass
	return 0.0, False


def content_tokens(text):
	return {t for t in re.findall(r"[a-z0-9]+", text.lower()) if len(t) >= 3 and t not in STOP}


def coverage(target, haystack):
	"""Fraction of the target's content tokens present in the haystack."""
	want = content_tokens(target)
	if not want:
		return 0.0
	return len(want & content_tokens(haystack)) / len(want)


def score_question(q, context_lines, args, answer_ep, judge_ep, rec, counts):
	"""Fill rec with score + provenance for one question, mode-dependent."""
	if args.mode == "retrieval":
		# No LLM anywhere: coverage of the ideal answer's content tokens in
		# the retrieved context. IF/PF ship no ideal, so their rubric text
		# stands in; either way the report labels this a proxy.
		target = q["ideal"] or " ".join(q["rubric"])
		rec["score"] = coverage(target, " ".join(context_lines))
		rec["judge"] = "coverage-proxy"
		return

	t0 = time.monotonic()
	answer = answer_question(answer_ep, q["question"], context_lines)
	rec["answer_secs"] = time.monotonic() - t0
	t0 = time.monotonic()
	value, ok = judge_answer(judge_ep, q["question"], q["rubric"], answer)
	rec["judge_secs"] = time.monotonic() - t0
	if ok:
		rec["judge"] = "rubric-json"
	elif args.fake_llm:
		# The fake echoes prompts, so its "judgment" never parses. Fall back
		# to token overlap purely so the smoke run exercises non-zero
		# aggregation — allowed ONLY under --fake-llm, whose report already
		# declares itself MEANINGLESS.
		value = coverage(" ".join(q["rubric"]), answer)
		rec["judge"] = "fake-echo-overlap"
	else:
		# Real run, unparseable judge: score 0 and count it loudly. A
		# similarity fallback here would quietly re-inflate exactly the way
		# the mnemosyne postmortem warns about.
		counts["judge_unparseable"] += 1
		rec["judge"] = "unparseable"
	rec["score"] = value
	rec["answer"] = answer[:300]


def run_conversation(kern_bin, conv, scale, args, embed, llm_url, answer_ep, judge_ep, counts, latencies, done):
	todo = conv["questions"]
	if args.max_questions:
		todo = todo[: args.max_questions]
	qids = [f"{scale}:{conv['id']}:q{i}" for i in range(len(todo))]
	if all(qid in done for qid in qids):
		counts["conversations_resumed"] += 1
		return []

	records = []
	with tempfile.TemporaryDirectory(prefix="kern-beam-") as tmp:
		tmp = Path(tmp)
		project = make_project(kern_bin, tmp, embed, llm_url, args.k)
		ingest_conversation(project, conv["sessions"], tmp, counts)
		cache = {}
		for qid, q in zip(qids, todo):
			if qid in done:
				continue
			context_lines, secs = retrieve_context(project, q["question"], cache, args.k)
			latencies.append(secs)
			rec = {
				"qid": qid,
				"scale": scale,
				"ability": q["ability"],
				"question": q["question"][:200],
				"hits": len(context_lines),
			}
			score_question(q, context_lines, args, answer_ep, judge_ep, rec, counts)
			records.append(rec)
		project.kill_all()
	return records


def checkpoint_config(args, answer_ep, judge_ep):
	return {
		"mode": args.mode,
		"embed_model": "fake-embed" if args.fake_llm else args.embed_model,
		"answer_model": answer_ep[1] if args.mode == "end_to_end" else None,
		"judge_model": judge_ep[1] if args.mode == "end_to_end" else None,
		"k": args.k,
	}


def load_checkpoint(path, config, resume):
	if not resume:
		return []
	if not path.exists():
		print(f"resume: no checkpoint at {path}, starting fresh")
		return []
	data = json.loads(path.read_text())
	if data.get("config") != config:
		# Mixing configurations in one score table is silent corruption, the
		# exact class of bug this runner exists to avoid — refuse.
		raise SystemExit(
			f"resume: checkpoint config {data.get('config')} != current {config}; "
			f"delete {path} or rerun with the original flags"
		)
	records = data.get("records", [])
	print(f"resume: {len(records)} questions already scored from {path}")
	return records


def save_checkpoint(path, config, records):
	path.parent.mkdir(parents=True, exist_ok=True)
	path.write_text(json.dumps({"config": config, "records": records}, indent=1))


def ability_table(records):
	by = defaultdict(list)
	for r in records:
		by[(r["scale"], r["ability"])].append(r["score"])
	table = {}
	for sc in sorted({s for s, _ in by}):
		row = {}
		everything = []
		for ab in ABILITIES:
			vals = by.get((sc, ab), [])
			if vals:
				row[ab] = {"score": sum(vals) / len(vals), "n": len(vals)}
				everything.extend(vals)
		row["OVERALL"] = {
			"score": sum(everything) / len(everything) if everything else 0.0,
			"n": len(everything),
		}
		table[sc] = row
	return table


def print_table(table):
	header = f"{'scale':<8}{'OVERALL':>9}" + "".join(f"{ab:>7}" for ab in ABILITIES)
	print(header)
	for sc, row in table.items():
		cells = f"{sc:<8}{row['OVERALL']['score'] * 100:>8.1f}%"
		for ab in ABILITIES:
			cells += f"{row[ab]['score'] * 100:>6.1f}%" if ab in row else f"{'—':>7}"
		print(cells)


def main():
	parser = argparser(__doc__)
	parser.add_argument("--data-dir", type=Path, default=DATA_DIR)
	parser.add_argument(
		"--scales", default="100K", help=f"comma-separated subset of {','.join(SCALES)}"
	)
	parser.add_argument(
		"--sample", type=int, default=3, help="conversations per scale (0 = all)"
	)
	parser.add_argument("--mode", choices=["retrieval", "end_to_end"], default="end_to_end")
	parser.add_argument(
		"--max-questions",
		type=int,
		default=0,
		help="cap probing questions per conversation (0 = all)",
	)
	parser.add_argument(
		"--llm-url", default=None, help="answer completion endpoint (default: --embed-url)"
	)
	parser.add_argument("--llm-model", default="qwen3.5:4b", help="answer completion model")
	parser.add_argument(
		"--judge-url", default=None, help="judge completion endpoint (default: --llm-url)"
	)
	parser.add_argument(
		"--judge-model",
		default=None,
		help="judge model (default: --llm-model); always reported separately",
	)
	parser.add_argument(
		"--resume",
		action="store_true",
		help="skip questions already in the checkpoint (must match mode/models/k)",
	)
	args = parse_args(parser)

	scales = [s.strip() for s in args.scales.split(",") if s.strip()]
	bad = [s for s in scales if s not in SCALES]
	if bad:
		raise SystemExit(
			f"unknown scales {bad}; supported: {', '.join(SCALES)} "
			"(the 10M tier is a separate HF repo and not wired up)"
		)
	data_paths = {s: args.data_dir / f"beam_{s}.json" for s in scales}
	missing = [str(p) for p in data_paths.values() if not p.exists()]
	if missing:
		raise SystemExit(f"{', '.join(missing)} missing — run `just eval-fetch beam` first")

	embed, llm_url, closer = open_endpoints(args)
	if args.fake_llm:
		answer_ep = judge_ep = (llm_url, "fake-echo")
	else:
		a_url = args.llm_url or args.embed_url
		answer_ep = (a_url, args.llm_model)
		judge_ep = (args.judge_url or a_url, args.judge_model or args.llm_model)
	if args.mode == "end_to_end" and not args.fake_llm:
		print(f"answer endpoint {answer_ep[0]} model {answer_ep[1]}")
		print(f"judge  endpoint {judge_ep[0]} model {judge_ep[1]}")

	config = checkpoint_config(args, answer_ep, judge_ep)
	checkpoint = args.report_dir / f"beam-{args.mode}-checkpoint.json"
	records = load_checkpoint(checkpoint, config, args.resume)
	done = {r["qid"] for r in records}

	kern_bin = build_kern()
	counts = defaultdict(int)
	counts["resumed_questions"] = len(done)
	latencies = []
	try:
		for sc in scales:
			convs = load_conversations(data_paths[sc], counts)
			if args.sample and args.sample < len(convs):
				print(f"LIMIT: {sc}: first {args.sample} of {len(convs)} conversations (--sample)")
				convs = convs[: args.sample]
			for i, conv in enumerate(convs):
				records.extend(
					run_conversation(
						kern_bin, conv, sc, args, embed, llm_url,
						answer_ep, judge_ep, counts, latencies, done,
					)
				)
				save_checkpoint(checkpoint, config, records)
				scored = sum(1 for r in records if r["scale"] == sc)
				print(f"{sc} conversation {i + 1}/{len(convs)}: {scored} questions scored")
	finally:
		closer()

	current = [r for r in records if r["scale"] in scales]
	report = base_report(args, f"BEAM ({', '.join(scales)})")
	if args.mode == "end_to_end":
		report["protocol"] = (
			"BEAM end-to-end: direct-path `kern ingest` per session, `kern query` "
			"per probing question, hits resolved to full stored text via `kern get`, "
			"answer LLM sees retrieved context ONLY (no raw messages, no harness "
			"oracles), judge LLM scores against the dataset rubric"
		)
		report["comparable_to"] = (
			"LLM-judged BEAM per-ability scores (rubric/nugget protocol) — the "
			"answer and judge models change the number and must be quoted with it; "
			"NOT comparable to kern's retrieval-only recall@k reports"
		)
		report["answer_model"] = f"{answer_ep[1]} @ {answer_ep[0]}"
		report["judge_model"] = f"{judge_ep[1]} @ {judge_ep[0]}"
	else:
		report["protocol"] = (
			"BEAM retrieval proxy: direct-path `kern ingest` per session, `kern "
			"query` per probing question; score is ideal-answer content-token "
			"coverage in the retrieved context — no LLM anywhere"
		)
		report["comparable_to"] = (
			"nothing published — a plumbing/ablation proxy, NOT an LLM-judged BEAM score"
		)
	report["mode"] = args.mode
	report["scales"] = scales
	report["sample"] = args.sample or "all"
	report["max_questions"] = args.max_questions or "all"
	report["counts"] = dict(counts)
	report["by_scale"] = ability_table(current)
	report["query_latency_secs"] = {
		"note": "cold-process CLI wall clock: spawn + graph load + embed + retrieve",
		"p50": score.percentile(latencies, 50),
		"p95": score.percentile(latencies, 95),
	}
	if args.mode == "end_to_end":
		answer_secs = [r["answer_secs"] for r in current if "answer_secs" in r]
		judge_secs = [r["judge_secs"] for r in current if "judge_secs" in r]
		report["answer_latency_secs"] = {
			"p50": score.percentile(answer_secs, 50),
			"p95": score.percentile(answer_secs, 95),
		}
		report["judge_latency_secs"] = {
			"p50": score.percentile(judge_secs, 50),
			"p95": score.percentile(judge_secs, 95),
		}

	name = "beam" if args.mode == "end_to_end" else "beam-retrieval"
	path = write_report(args.report_dir, name, report)
	print_table(report["by_scale"])
	if counts["judge_unparseable"]:
		print(
			f"WARNING: {counts['judge_unparseable']} judge replies were unparseable "
			"and scored 0 — the judge model is not holding the JSON contract"
		)
	print(f"report: {path}")


if __name__ == "__main__":
	main()
