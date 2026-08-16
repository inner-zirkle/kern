"""Fetch the benchmark datasets into tests/eval/ (gitignored, never committed).

LoCoMo is CC BY-NC 4.0, LongMemEval is research data, and BEAM is CC BY-SA
4.0 — all are downloaded to the user's machine on demand and stay out of the
repo.
"""

import argparse
import json
import sys
import urllib.request
from pathlib import Path

from common import DATA_DIR

LOCOMO_URL = (
	"https://raw.githubusercontent.com/snap-research/locomo/main/data/locomo10.json"
)
LOCOMO_PATH = DATA_DIR / "locomo10.json"

# The HF repo ships the files with no .json extension; the content is JSON.
LONGMEMEVAL_REPO = "xiaowu0162/longmemeval"
LONGMEMEVAL_FILE = "longmemeval_s"
LONGMEMEVAL_PATH = DATA_DIR / LONGMEMEVAL_FILE

# BEAM ships parquet-only; each split is converted once, at fetch time, into
# beam_<scale>.json so run_beam.py needs nothing beyond the stdlib. The 10M
# tier is a separate streaming-scale repo (Mohammadta/BEAM-10M), not wired up.
BEAM_REPO = "Mohammadta/BEAM"
BEAM_SCALES = ("100K", "500K", "1M")

NOTICE = """\
Datasets are for local evaluation only:
- LoCoMo (snap-research/locomo): CC BY-NC 4.0 — non-commercial, no redistribution.
- LongMemEval (xiaowu0162/longmemeval): research benchmark, see its license.
- BEAM (Mohammadta/BEAM): CC BY-SA 4.0, see its dataset card.
All live under tests/eval/, which is gitignored; do not commit them."""


def fetch_locomo():
	if LOCOMO_PATH.exists():
		print(f"locomo: already at {LOCOMO_PATH}")
		return
	DATA_DIR.mkdir(exist_ok=True)
	print(f"locomo: downloading {LOCOMO_URL}")
	urllib.request.urlretrieve(LOCOMO_URL, LOCOMO_PATH)
	print(f"locomo: {LOCOMO_PATH} ({LOCOMO_PATH.stat().st_size:,} bytes)")


def fetch_longmemeval():
	if LONGMEMEVAL_PATH.exists():
		print(f"longmemeval: already at {LONGMEMEVAL_PATH}")
		return
	try:
		from huggingface_hub import hf_hub_download
	except ImportError:
		sys.exit("longmemeval needs huggingface-hub: just e2e-install")
	DATA_DIR.mkdir(exist_ok=True)
	print(f"longmemeval: downloading {LONGMEMEVAL_FILE} from {LONGMEMEVAL_REPO}")
	got = hf_hub_download(
		repo_id=LONGMEMEVAL_REPO,
		filename=LONGMEMEVAL_FILE,
		repo_type="dataset",
		local_dir=DATA_DIR,
	)
	print(f"longmemeval: {got} ({Path(got).stat().st_size:,} bytes)")


def fetch_beam(scales):
	try:
		from huggingface_hub import hf_hub_download
	except ImportError:
		sys.exit("beam needs huggingface-hub: just e2e-install")
	for scale in scales:
		out = DATA_DIR / f"beam_{scale}.json"
		if out.exists():
			print(f"beam: already at {out}")
			continue
		try:
			import pyarrow.parquet as pq
		except ImportError:
			sys.exit("beam is parquet-only on the hub; converting needs pyarrow: pip install pyarrow")
		DATA_DIR.mkdir(exist_ok=True)
		fname = f"data/{scale}-00000-of-00001.parquet"
		print(f"beam: downloading {fname} from {BEAM_REPO}")
		got = hf_hub_download(
			repo_id=BEAM_REPO,
			filename=fname,
			repo_type="dataset",
			local_dir=DATA_DIR / "beam_parquet",
		)
		rows = pq.read_table(got).to_pylist()
		# Keep only what a runner may ever see: the conversation itself and
		# its probing questions. The generator-side columns (narratives,
		# conversation_plan, user_profile, user_questions) are answer-adjacent
		# material — dropping them at fetch time means no harness code can
		# leak them into a prompt (mnemosyne postmortem: no oracles).
		slim = [
			{
				"conversation_id": r["conversation_id"],
				"chat": r["chat"],
				"probing_questions": r["probing_questions"],
			}
			for r in rows
		]
		out.write_text(json.dumps(slim))
		print(f"beam: {out} ({out.stat().st_size:,} bytes, {len(slim)} conversations)")


def main():
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument(
		"which",
		nargs="?",
		default="all",
		choices=["locomo", "longmemeval", "beam", "all"],
	)
	parser.add_argument(
		"--beam-scales",
		default="100K",
		help=f"comma-separated subset of {','.join(BEAM_SCALES)} (default 100K; "
		"the larger splits are 86MB/172MB parquet downloads)",
	)
	args = parser.parse_args()
	which = args.which
	print(NOTICE)
	if which in ("locomo", "all"):
		fetch_locomo()
	if which in ("longmemeval", "all"):
		fetch_longmemeval()
	if which in ("beam", "all"):
		scales = [s.strip() for s in args.beam_scales.split(",") if s.strip()]
		bad = [s for s in scales if s not in BEAM_SCALES]
		if bad:
			sys.exit(f"unknown beam scales {bad}; supported: {', '.join(BEAM_SCALES)}")
		fetch_beam(scales)


if __name__ == "__main__":
	main()
