"""Per-source TTL end to end: `kern ingest --retention-secs N` is the writer half,
`retrieval::score::drop_expired` is the reader half. Ingest a fact with a short
retention, prove `kern query` returns it before the deadline and drops it after,
while a fact ingested without the flag keeps coming back.

Wall-clock is load-bearing here — `valid_until` is an absolute instant compared
against `SystemTime::now()` on every delivery, so the test sleeps rather than
mocking. RETENTION is kept short and the durable control fact carries the whole
"the graph is still there" half, so a slow box costs seconds, not a false pass.
"""

import time

from ranking import full_id, hits

EPHEMERAL = "The pager rotation this week belongs to Ada"
DURABLE = "Marrow the cat refuses to eat anything except salmon"
ID_PROBE = "The oncall handset lives in the second drawer of the Helsinki desk"

# DEDUPED_NEAR is the same token bag as DEDUPED, so the fake embedder returns an
# identical vector and `find_duplicate` — the FIRST dedup gate — merges it, while
# the differing case and trailing period give it a different content hash, which
# is what makes the ingest report `deduped` rather than a no-op re-commit. The
# second gate is unreachable from here (both entities sit in `entity_idx`, so the
# first gate always wins); it is covered by
# `place::tests::the_second_dedup_gate_tightens_too_and_orphans_no_delta`.
DEDUPED = "Vault rotation for the staging cluster is owned by Rui"
DEDUPED_NEAR = "vault rotation for the staging cluster is owned by rui."

# The intake half: these arrive as `.txt` transcripts under a source policy, not
# as `kern ingest` arguments, so no `--retention-secs` is ever passed for them.
INTAKE_EPHEMERAL = "The spare archive key is taped beneath the third shelf"
INTAKE_DURABLE = "Ada keeps her bicycle in the garden shed behind the house"

RETENTION = 5


def texts_for(project, probe):
	stdout, stderr = project.run("query", probe)
	return [hit.text for hit in hits(stdout)], stdout, stderr


def wait_past_deadline(seconds):
	"""Block until CLOCK_REALTIME has advanced `seconds`, not the monotonic clock.

	`valid_until` is an absolute instant the reader compares against
	`SystemTime::now()`, so realtime is the clock that has to move. `time.sleep`
	waits on the monotonic one, and a host that steps realtime backwards leaves a
	monotonic sleep short — WSL2's hv time sync does, running realtime ~3.8% slow
	and repaying the whole accrued drift in one jump per ~32s. The rate is the
	constant, not the step, so no fixed margin over a fixed sleep is safe: waiting
	longer loses proportionally more, and a delayed sync bunches the loss.

	The target is absolute, so a backward step needs no special case — only
	realtime reaching it ends the wait. The cap is monotonic so a stopped clock
	fails loudly instead of hanging.
	"""
	target = time.time() + seconds
	cap = time.monotonic() + 4 * seconds + 30
	while time.time() < target:
		assert time.monotonic() < cap, (
			"the realtime clock never advanced past the retention deadline"
		)
		time.sleep(0.25)


def wait_until_dropped(project, probe, text):
	"""Poll until `text` stops being delivered; return the last query output.

	Polling rather than one check after a fixed wait: the same backward realtime
	steps can land BETWEEN the wait and the query and put an already-passed
	deadline back in the future. The monotonic cap keeps a genuine failure to
	expire a failure — it just refuses to read clock noise as one.
	"""
	cap = time.monotonic() + 4 * RETENTION + 30
	while True:
		found, stdout, stderr = texts_for(project, probe)
		if text not in found:
			return stdout, stderr
		assert time.monotonic() < cap, (
			f"{text!r} still delivered well past its retention deadline: "
			f"out={stdout} err={stderr}"
		)
		time.sleep(0.5)


def test_a_retention_expires_the_fact_out_of_query_results(project):
	stdout, stderr = project.run("ingest", EPHEMERAL, "--retention-secs", str(RETENTION))
	assert "status=committed" in stdout, f"ttl ingest failed: out={stdout} err={stderr}"
	stdout, stderr = project.run("ingest", DURABLE)
	assert "status=committed" in stdout, f"control ingest failed: out={stdout} err={stderr}"

	found, stdout, stderr = texts_for(project, EPHEMERAL)
	assert EPHEMERAL in found, (
		f"a fact with an unexpired retention must still be recallable: out={stdout} err={stderr}"
	)

	wait_past_deadline(RETENTION)
	wait_until_dropped(project, EPHEMERAL, EPHEMERAL)

	found, stdout, stderr = texts_for(project, DURABLE)
	assert DURABLE in found, (
		f"expiry took the whole graph with it: out={stdout} err={stderr}"
	)


def test_an_expired_fact_is_served_by_id_but_flagged(project):
	"""The id surface is the second read surface and it has no `drop_expired` in
	front of it. Item 22 closed green while `kern get <id>` served an expired fact
	as if it were current, because this file only ever asked the ranked path.

	The ranked path drops; the id path answers a named row, so it answers — with
	the expiry stated. A test that only checked the ranked path here would pass
	with the id path still lying, which is exactly the gap being closed.
	"""
	stdout, stderr = project.run("ingest", ID_PROBE, "--retention-secs", str(RETENTION))
	assert "status=committed" in stdout, f"ttl ingest failed: out={stdout} err={stderr}"

	# Captured before the deadline: recall is the only printer of a short id, and
	# resolving one after expiry is the thing under test, not a precondition of it.
	thought_id = full_id(project, ID_PROBE)

	got, stderr = project.run("get", thought_id)
	assert "Expired:" not in got, (
		f"nothing is expired before its deadline: out={got} err={stderr}"
	)

	wait_past_deadline(RETENTION)
	wait_until_dropped(project, ID_PROBE, ID_PROBE)

	got, stderr = project.run("get", thought_id)
	assert ID_PROBE in got, (
		"an explicit id must not be answered with 'thought not found' for a row that "
		f"is still on disk: out={got} err={stderr}"
	)
	assert "Expired:" in got, (
		f"the id path served an expired fact as if it were current: out={got} err={stderr}"
	)


def test_a_deduped_ingest_still_applies_its_retention(project):
	"""A retention-carrying ingest that lands on a near-duplicate must tighten the
	SURVIVOR's deadline. It reports `deduped` — the incoming entity is dropped
	whole — so before item 88 the flag silently did nothing on this path and left
	an entity that never expires.
	"""
	stdout, stderr = project.run("ingest", DEDUPED)
	assert "status=committed" in stdout, f"seed ingest failed: out={stdout} err={stderr}"

	found, stdout, stderr = texts_for(project, DEDUPED)
	assert DEDUPED in found, f"seed fact not recallable: out={stdout} err={stderr}"

	stdout, stderr = project.run("ingest", DEDUPED_NEAR, "--retention-secs", str(RETENTION))
	assert "status=deduped" in stdout, (
		f"the near-duplicate must merge, not place: out={stdout} err={stderr}"
	)

	found, stdout, stderr = texts_for(project, DEDUPED)
	assert DEDUPED in found, (
		f"the survivor must still be recallable before the deadline: out={stdout} err={stderr}"
	)

	wait_past_deadline(RETENTION)
	wait_until_dropped(project, DEDUPED, DEDUPED)


def queue_transcript(project, name, claim):
	"""Drop a `.txt` session transcript the fake reason model distils into `claim`.

	`fake_llm.distilled` turns every `assistant:` line into one claim whose text
	is that line, so the stored claim is predictable enough to query for.
	"""
	intake = project.cwd / ".kern" / "intake"
	intake.mkdir(parents=True, exist_ok=True)
	(intake / name).write_text(f"user: tell me something\nassistant: {claim}\n")
	return intake


def test_a_source_retention_policy_expires_what_the_intake_drain_distills(project):
	"""The config half of item 89, at the entrance the flag cannot reach.

	Every other case in this file drives `kern ingest --retention-secs` — a
	per-call argument on an entrance that already worked. A queue has no caller
	to pass a flag: the transcript is a file someone dropped in a directory, so
	its TTL can only come from `[intake] retention_secs`. Before this landed,
	`drain_entry` built its per-claim config from a queue config nothing ever
	put a deadline on, and a distilled claim never expired.
	"""
	project.write_config(intake_retention_secs=RETENTION)
	intake = queue_transcript(project, "sess-ttl.txt", INTAKE_EPHEMERAL)

	stdout, stderr = project.run("intake", "drain")
	assert "drained 1 of 1 pending" in stdout, f"out={stdout} err={stderr}"
	assert (intake / "done" / "sess-ttl.txt").exists(), f"not archived: {stdout}"

	found, stdout, stderr = texts_for(project, INTAKE_EPHEMERAL)
	assert INTAKE_EPHEMERAL in found, (
		f"the distilled claim never reached the graph: out={stdout} err={stderr}"
	)

	wait_past_deadline(RETENTION)
	wait_until_dropped(project, INTAKE_EPHEMERAL, INTAKE_EPHEMERAL)


def test_an_intake_drain_with_no_policy_sets_no_ttl(project):
	"""The control: absent `retention_secs`, a drained transcript is permanent.

	Without it, a drain that expired everything would pass the test above.
	"""
	intake = queue_transcript(project, "sess-forever.txt", INTAKE_DURABLE)

	stdout, stderr = project.run("intake", "drain")
	assert "drained 1 of 1 pending" in stdout, f"out={stdout} err={stderr}"
	assert (intake / "done" / "sess-forever.txt").exists(), f"not archived: {stdout}"

	wait_past_deadline(RETENTION + 2)

	found, stdout, stderr = texts_for(project, INTAKE_DURABLE)
	assert INTAKE_DURABLE in found, (
		f"an unconfigured queue must stamp no valid_until: out={stdout} err={stderr}"
	)


def test_a_default_ingest_never_expires(project):
	stdout, stderr = project.run("ingest", DURABLE)
	assert "status=committed" in stdout, f"ingest failed: out={stdout} err={stderr}"

	wait_past_deadline(RETENTION + 2)

	found, stdout, stderr = texts_for(project, DURABLE)
	assert DURABLE in found, (
		f"an ingest without --retention-secs must set no valid_until: out={stdout} err={stderr}"
	)
