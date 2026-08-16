"""ROADMAP item 21: the review lifecycle, end to end through the binary.

The hold and the release are one feature and neither half is worth anything
alone — a host that can hold a claim but never release it strands it, and a
`promote` with no way to filter releases something nobody could have hidden. So
this walks the whole loop: `[ingest] review_policy = { inline = "pending" }`
holds every CLI ingest, `query --exclude-pending` must miss it, `kern promote`
must release it, and the same query must then return it.

None of this needs a JSON-RPC client: the policy
is ordinary config and both verbs are subprocess-visible, so the routed path and
the NoDaemon fallback are each measurable. Both are measured, for the reason
`test_graviton_routing` gives — a mutating CLI path that writes locally beside a
serving daemon releases the claim in a copy the daemon's next persist overwrites,
and the row reads as promoted while it is still held.
"""

import sys

import pytest

from ranking import full_id, hits

pytestmark = pytest.mark.skipif(sys.platform == "win32", reason="unix sockets only")

HELD = "Ada keeps her bicycle in the garden shed behind the house"
PROBE = "where does ada store her bicycle"
POLICY = {"inline": "pending"}
BLIND = ".kern/blind"


def found(stdout):
	return any(hit.text == HELD for hit in hits(stdout))


def ingest_held(project):
	"""Ingest the held claim and return its full id.

	`kern ingest` is `Source::Inline`, which the policy holds — and `search` is a
	raw ANN scan that answers no review filter, so it can still find the row to
	resolve an id from.
	"""
	stdout, stderr = project.run("ingest", HELD)
	assert "status=committed" in stdout, f"ingest failed: out={stdout} err={stderr}"
	return full_id(project, HELD)


def test_a_held_claim_is_filtered_until_promote_releases_it(project):
	"""The NoDaemon path: nothing serving, the CLI owns both the read and the write."""
	project.write_config(review_policy=POLICY, intake_enabled=False)
	held_id = ingest_held(project)

	# Opt-in, in both directions. A row that the unfiltered query never returned
	# would make the filtered miss below prove nothing.
	stdout, stderr = project.run("query", PROBE)
	assert found(stdout), f"the ordinary query still serves a held row: out={stdout} err={stderr}"

	stdout, stderr = project.run("query", PROBE, "--exclude-pending")
	assert not found(stdout), f"the policy held nothing: out={stdout} err={stderr}"

	stdout, stderr = project.run("promote", held_id)
	assert stdout.startswith("promoted "), f"promote failed: out={stdout} err={stderr}"
	assert "already active" not in stdout, f"the row was never held: {stdout}"

	stdout, stderr = project.run("query", PROBE, "--exclude-pending")
	assert found(stdout), f"promote did not release the claim: out={stdout} err={stderr}"

	# Idempotent: a curator who retries is told nothing changed, not that it failed.
	stdout, stderr = project.run("promote", held_id)
	assert "already active" in stdout, f"re-promote must not error: out={stdout} err={stderr}"

	# And an id nothing resolves is loud — a silent success here would tell a
	# curator a claim was released while it is still held.
	code, stdout, stderr = project.run_status("promote", "0" * 64)
	assert "no thought with id" in stderr, f"a mistyped id must fail: out={stdout} err={stderr}"
	assert code != 0, f"and must fail in the status too, not only in the text: {stderr}"


def test_promote_and_the_filter_route_through_a_serving_daemon(project):
	"""The routed path, blinded the way test_graviton_routing blinds it.

	A daemon reads its config once and then holds its store open, so repointing
	`data_dir` afterwards leaves every later CLI process reading an empty
	directory. From that point anything printed came over the socket.
	"""
	project.write_config(review_policy=POLICY, intake_enabled=False)
	held_id = ingest_held(project)

	daemon = project.start_daemon()
	project.write_config(data_dir=BLIND, review_policy=POLICY, intake_enabled=False)

	# Control: `list` stays local by decision, so its going blind is what proves
	# the repoint took and that the disk under this CLI really is empty.
	stdout, _ = project.run("list")
	assert "bicycle" not in stdout, f"the local store is not blind: {stdout}"

	stdout, stderr = project.run("query", PROBE)
	assert found(stdout), f"routed query returned nothing: out={stdout} err={stderr}"

	stdout, stderr = project.run("query", PROBE, "--exclude-pending")
	assert not found(stdout), (
		f"the daemon did not honour the held state: out={stdout} err={stderr}"
	)

	stdout, stderr = project.run("promote", held_id)
	assert stdout.startswith("promoted "), f"routed promote failed: out={stdout} err={stderr}"
	assert "already active" not in stdout, f"the daemon's row was never held: {stdout}"

	stdout, stderr = project.run("query", PROBE, "--exclude-pending")
	assert found(stdout), (
		f"the routed promote never reached the daemon's live graph: out={stdout} err={stderr}"
	)

	# Kill it and unblind: the release has to have survived the daemon's own
	# persist, or it only ever existed in RAM — and the same query now falls
	# through to the local store rather than erroring on the dead socket.
	project.stop(daemon)
	project.write_config(review_policy=POLICY, intake_enabled=False)
	stdout, stderr = project.run("query", PROBE, "--exclude-pending")
	assert found(stdout), (
		f"the daemon's persist dropped the promotion: out={stdout} err={stderr}"
	)
