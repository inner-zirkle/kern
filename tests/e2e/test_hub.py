"""Hub supervisor e2e over a real socket, isolated by a private
XDG_RUNTIME_DIR. Unix-only: named pipes have no per-test namespace.
Folded in from the retired Rust suite e2e/hub_supervisor.rs."""

import json
import subprocess
import sys

import pytest

from conftest import wait_until

pytestmark = pytest.mark.skipif(sys.platform == "win32", reason="unix sockets only")


def test_hub_binds_serves_status_and_rejects_bogus_unload(project):
	project.start_hub()

	stdout, _ = project.run("hub", "status")
	assert "no nodes" in stdout, f"fresh hub tracks nothing: {stdout}"

	_, stderr = project.run("hub", "unload", "/nonexistent/kern-hub-test")
	assert "/nonexistent/kern-hub-test" in stderr, (
		f"bogus root is rejected with the path named: {stderr}"
	)

	stdout, stderr = project.run("hub", "unload")
	assert "no node for" in stdout, f"nodeless unload is a no-op: out={stdout} err={stderr}"


def test_resolve_spawns_a_node_and_unload_reaps_it(project):
	project.start_hub()

	stdout, stderr = project.run("hub", "resolve")
	assert "spawned" in stdout, f"first resolve cold-boots a node: out={stdout} err={stderr}"

	stdout, _ = project.run("hub", "resolve")
	assert "running" in stdout, f"second resolve reuses the node: {stdout}"

	stdout, _ = project.run("hub", "status")
	assert "up" in stdout and "kern-" in stdout, f"status lists the live node: {stdout}"

	stdout, stderr = project.run("hub", "unload")
	assert "unloaded" in stdout, f"unload succeeds: out={stdout} err={stderr}"

	wait_until(
		lambda: not project.node_sockets(),
		10,
		"node socket lingered after unload",
	)


def test_an_idle_node_is_auto_unloaded(project):
	project.start_hub("--idle-unload-secs", "2")

	stdout, stderr = project.run("hub", "resolve")
	assert "spawned" in stdout, f"resolve boots a node: out={stdout} err={stderr}"

	# Never queried after boot -> idle from startup; the 2s threshold with the
	# matched reaper cadence must clear it well inside the window. Poll the socket
	# rather than shelling out to `hub status`: each poll would spawn a process,
	# and on a loaded CI runner the polling cost, not the reaper, is what runs the
	# clock out. This is an eventual property — the deadline bounds the wait, it
	# does not measure the latency.
	wait_until(
		lambda: not project.node_sockets(),
		30,
		"idle node was never unloaded",
	)
	stdout, _ = project.run("hub", "status")
	assert "no nodes" in stdout, f"the reaped node is gone from status too: {stdout}"


def test_a_second_hub_refuses_the_taken_socket(project):
	project.start_hub()

	out = subprocess.run(
		[project.bin, "hub"],
		cwd=project.cwd,
		env=project.env,
		capture_output=True,
		text=True,
		timeout=30,
	)
	assert "already running" in out.stderr, (
		f"second hub must refuse and exit: {out.stderr}"
	)


def test_kern_mcp_auto_starts_the_hub_and_routes_through_it(project):
	# One MCP handshake with NO hub running: the proxy must auto-start one,
	# resolve through it, and answer initialize.
	child = project.spawn("mcp", stdin=subprocess.PIPE, stdout=subprocess.PIPE)
	try:
		req = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}
		child.stdin.write((json.dumps(req) + "\n").encode())
		child.stdin.flush()
		# Reading the initialize response proves the whole attach path ran:
		# hub auto-started, node resolved (spawned), proxy connected.
		line = child.stdout.readline().decode()
		assert '"protocolVersion"' in line, f"proxy must answer initialize: {line}"
		assert (project.runtime / "kern-hub.sock").exists(), (
			"kern mcp must leave a machine hub behind"
		)
	finally:
		child.kill()
		child.wait()

	# The hub (not the proxy) owns the node: status from a fresh process sees it.
	wait_until(
		lambda: "up" in project.run("hub", "status")[0],
		15,
		"hub never tracked the node",
	)

	# Cleanup: unload the node and stop the detached hub so nothing outlives the test.
	project.run("hub", "unload")
	stdout, stderr = project.run("hub", "stop")
	assert "hub stopped" in stdout, (
		f"detached hub must stop via RPC: out={stdout} err={stderr}"
	)


def test_status_without_a_hub_fails_softly(project):
	"""Softly = it answers and exits, rather than hanging on a socket nobody
	binds. It still exits non-zero: the caller asked for a hub's status and
	there is no hub, which is a question left unanswered, not an answer."""
	code, _, stderr = project.run_status("hub", "status")
	assert "no hub running" in stderr, f"no hub -> soft failure, not a hang: {stderr}"
	assert code != 0, f"an unanswered status must not exit 0: {stderr}"
