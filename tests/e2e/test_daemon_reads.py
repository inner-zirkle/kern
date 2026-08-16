"""The read half of ROADMAP item 9: `get` and `query` go through a serving
daemon, and fall back to the local store only when nothing is serving.

The proof needs the daemon's answer to be one no local read could have produced.
A daemon reads its config once, at startup, and then holds its store open — so
rewriting `.kern/kern.toml` to name an empty `data_dir` blinds every *later* CLI
process without moving the graph the daemon is already serving. From that point
on, anything the CLI prints came over the socket.

`query --mode vector` and `list` stay local by decision (item 9 notes), which is
what makes them the control here: with the config repointed they must go blind in
the same breath that `get` and a full `query` still answer.
"""

import sys

import pytest

from ranking import full_id, hits, ingest_all
# Borrowed as a corpus, not as a test: the parity check below needs more facts
# than the query tool's own default k, or the cut it guards never happens.
from test_recall import FACTS as BIG_CORPUS

pytestmark = pytest.mark.skipif(sys.platform == "win32", reason="unix sockets only")

BIKE = "Ada keeps her bicycle in the garden shed behind the house"
FACTS = [
	BIKE,
	"The deploy pipeline runs on Jenkins every night at two",
	"Marrow the cat refuses to eat anything except salmon",
]

BLIND = ".kern/blind"


def test_get_and_query_read_the_serving_daemons_live_graph(project):
	ingest_all(project, FACTS)
	bike_id = full_id(project, BIKE)
	# Baseline for the control below: both local commands see the fact right now,
	# so their going blind after the repoint means something.
	before, _ = project.run("list")
	assert "bicycle" in before, f"list sees the store before the repoint: {before}"

	daemon = project.start_daemon()
	# The daemon holds the ingested graph in RAM; this CLI can no longer see the
	# store it came from.
	project.write_config(data_dir=BLIND)

	# Control: the two deliberately-local commands go blind, which is what proves
	# the repoint took and that the disk under this CLI really is empty.
	stdout, _ = project.run("query", "--mode", "vector", BIKE)
	assert not hits(stdout), f"vector recall must stay local and see nothing: {stdout}"
	stdout, _ = project.run("list")
	assert "bicycle" not in stdout, f"list must stay local and see nothing: {stdout}"

	# Routed: only the daemon's live graph can answer either of these now.
	stdout, stderr = project.run("get", bike_id)
	assert f"ID:     {bike_id}" in stdout, f"routed get: out={stdout} err={stderr}"
	assert "bicycle" in stdout, f"routed get printed no text: {stdout}"

	stdout, stderr = project.run("query", "where does ada store her bicycle")
	ranked = hits(stdout)
	assert ranked, f"routed query returned nothing: out={stdout} err={stderr}"
	assert any("bicycle" in hit.text for hit in ranked), f"wrong hits: {ranked}"

	# A prefix is all the CLI ever prints, so the routed lookup has to resolve one.
	stdout, stderr = project.run("get", bike_id[:12])
	assert f"ID:     {bike_id}" in stdout, f"routed prefix get: out={stdout} err={stderr}"

	# Kill it. Both commands must fall through to the local store rather than
	# erroring on the dead socket the daemon left behind — first while still
	# pointed at the empty dir (the store answers "not found", the socket does
	# not answer at all), then pointed back at the real one.
	project.stop(daemon)
	_, stderr = project.run("get", bike_id)
	assert f"kern get: no thought with id {bike_id}" in stderr, (
		f"NoDaemon get must read the local store, not the dead socket: {stderr}"
	)
	stdout, _ = project.run("query", "where does ada store her bicycle")
	assert "no results" in stdout, f"NoDaemon query must read the local store: {stdout}"

	project.write_config()
	stdout, stderr = project.run("get", bike_id)
	assert f"ID:     {bike_id}" in stdout, f"fallback get: out={stdout} err={stderr}"
	assert not stderr.strip(), f"the fallback must be silent, not degraded: {stderr}"
	stdout, stderr = project.run("query", "where does ada store her bicycle")
	assert any("bicycle" in hit.text for hit in hits(stdout)), (
		f"fallback query: out={stdout} err={stderr}"
	)


def test_get_and_query_answer_off_disk_with_no_daemon(project):
	"""The NoDaemon branch on a real store: nothing serving, both still work."""
	ingest_all(project, FACTS)
	bike_id = full_id(project, BIKE)

	stdout, stderr = project.run("get", bike_id)
	assert f"ID:     {bike_id}" in stdout, f"local get: out={stdout} err={stderr}"
	assert "bicycle" in stdout, f"local get printed no text: {stdout}"

	stdout, stderr = project.run("query", "where does ada store her bicycle")
	ranked = hits(stdout)
	assert ranked, f"local query returned nothing: out={stdout} err={stderr}"
	assert any("bicycle" in hit.text for hit in ranked), f"wrong hits: {ranked}"


def test_routed_query_delivers_as_many_hits_as_the_local_one(project):
	"""One command, one answer size.

	The `query` tool's own `k` default is `seed_k`, well under the delivery pool
	`kern query` prints locally — so a CLI that routes without naming `k` returns
	fewer hits whenever a daemon happens to be up. Same corpus, same probe, both
	paths: the counts have to match.
	"""
	ingest_all(project, BIG_CORPUS)
	probe = "where does ada store her bicycle"

	stdout, stderr = project.run("query", probe)
	local = hits(stdout)
	assert len(local) > 25, (
		f"corpus must exceed the tool's default k or this proves nothing: {stdout} {stderr}"
	)

	project.start_daemon()
	stdout, stderr = project.run("query", probe)
	routed = hits(stdout)
	assert len(routed) == len(local), (
		f"routed {len(routed)} hits vs local {len(local)}: out={stdout} err={stderr}"
	)
	assert routed[0].short_id == local[0].short_id, (
		f"top hit differs: routed={routed[0]} local={local[0]}"
	)
