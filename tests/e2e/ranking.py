"""Shared parsing and setup helpers for the retrieval e2e suite.

Every read mode of `query` prints one line per hit from the same println! in
src/commands/src/commands_query.rs:

    1. [0.3405] 14da0c1e89ae  Ada keeps her bicycle in the garden shed

`query` may then append a `--- Connections ---` block and
nothing else — the read path emits no synthesized block.
Those trailing sections are cut before parsing so a prompt echo can never be
counted as a ranked hit.
"""

import re
from collections import namedtuple

Hit = namedtuple("Hit", "rank score short_id text")

_HIT = re.compile(r"^(\d+)\. \[(-?\d+\.\d{4})\] (\S+)  (.*)$")
_ID = re.compile(r"^ID:\s+(\S+)$", re.M)


def hits(stdout):
	head = stdout.split("--- Connections ---", 1)[0]
	out = []
	for line in head.splitlines():
		m = _HIT.match(line)
		if m:
			out.append(Hit(int(m.group(1)), float(m.group(2)), m.group(3), m.group(4)))
	return out


def connections(stdout):
	if "--- Connections ---" not in stdout:
		return ""
	return stdout.split("--- Connections ---", 1)[1]


def ingest_all(project, texts):
	for text in texts:
		stdout, stderr = project.run("ingest", text)
		assert "status=committed" in stdout, (
			f"ingest failed for {text!r}: out={stdout} err={stderr}"
		)


def full_id(project, text, pool=64):
	"""Full 64-char id for an ingested text.

	query/list only ever print the 12-char short id, while link, degrade and
	forget match ids exactly — so every mutation has to round-trip through
	`get`, which does resolve a prefix.

	`--mode vector` is the bare nearest-neighbour read: no walk, no daemon, and
	an exact `--k`, which is what makes the pool here a pool and not a ranking.
	"""
	stdout, stderr = project.run("query", text, "--mode", "vector", "--k", str(pool))
	for hit in hits(stdout):
		if hit.text == text:
			got, _ = project.run("get", hit.short_id)
			m = _ID.search(got)
			assert m, f"get printed no ID line for {hit.short_id}: {got}"
			return m.group(1)
	raise AssertionError(f"{text!r} not in top {pool}: out={stdout} err={stderr}")


def link(project, from_text, to_text, reason):
	stdout, stderr = project.run(
		"link", full_id(project, from_text), full_id(project, to_text), "--reason", reason
	)
	assert stdout.startswith("linked "), f"link failed: out={stdout} err={stderr}"
	return stdout
