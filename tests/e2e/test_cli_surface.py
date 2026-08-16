"""The CLI's own contract, driven through the real binary.

Not retrieval and not the store: what a caller — a person at a prompt or a
script around one — can rely on. Three claims, each of which was false before:
a failure exits non-zero, an error names the command that produced it, and the
consolidated commands answer for the ones they absorbed.
"""

from ranking import full_id, hits, ingest_all

BIKE = "Ada keeps her bicycle in the garden shed behind the house"
DEPLOY = "The deploy pipeline runs on Jenkins every night at two"


def test_a_miss_exits_non_zero_and_names_the_command(project):
	ingest_all(project, [BIKE])

	code, stdout, stderr = project.run_status("get", "no-such-thought-id")
	assert code != 0, f"a miss that exits 0 is invisible to a script: out={stdout}"
	assert stderr.startswith("kern get:"), f"the error names its command: {stderr}"

	# The same command that just failed must still exit 0 when it succeeds, or
	# the status carries no information.
	bike_id = full_id(project, BIKE)
	code, stdout, stderr = project.run_status("get", bike_id)
	assert code == 0, f"a hit exits 0: out={stdout} err={stderr}"


def test_help_and_version_answer_before_anything_else(project):
	for args in (["--help"], ["-h"], ["--version"]):
		code, stdout, stderr = project.run_status(*args)
		assert code == 0, f"kern {' '.join(args)}: code={code} err={stderr}"
		assert stdout.strip(), f"kern {' '.join(args)} printed nothing"

	code, stdout, _ = project.run_status("--help")
	for cmd in ("ingest", "query", "forget", "status"):
		assert f"  {cmd}" in stdout, f"{cmd} is missing from the command list"
	# Every command carries a description: a blank column is what this replaced.
	for line in stdout.splitlines():
		if line.startswith("  ") and not line.startswith("    ") and line.strip():
			name, _, rest = line.strip().partition(" ")
			if name in ("kern", "Examples:", "Run"):
				continue
			assert rest.strip(), f"{name} has no description in --help"


def test_query_absorbed_the_vector_read_and_its_k(project):
	ingest_all(project, [BIKE, DEPLOY])

	code, stdout, stderr = project.run_status(
		"query", "--mode", "vector", "--k", "1", "where does ada keep her bicycle"
	)
	assert code == 0, f"vector recall failed: out={stdout} err={stderr}"
	ranked = hits(stdout)
	assert len(ranked) == 1, f"--k 1 must deliver exactly one hit: {ranked}"

	# The full pipeline honours --k too, so an answer's size does not depend on
	# which mode the caller happened to pick.
	_, stdout, _ = project.run_status("query", "--k", "1", "where does ada keep her bicycle")
	assert len(hits(stdout)) == 1, f"--k must bound the hybrid read as well: {stdout}"


def test_forget_absorbed_the_pattern_sweep_and_its_dry_run(project):
	ingest_all(project, [BIKE, DEPLOY])

	code, stdout, stderr = project.run_status("forget", "--match", "bicycle", "--dry-run")
	assert code == 0, f"a dry run is not a failure: out={stdout} err={stderr}"
	assert "would be removed" in stdout, f"the preview says what it would do: {stdout}"
	found = [h.text for h in hits(project.run("query", "--mode", "vector", BIKE)[0])]
	assert any("bicycle" in t for t in found), f"--dry-run removed something: {found}"

	# A CLI ingest lands as a Fact, and the Fact guard holds those without
	# --force — the same guard a single-id forget takes.
	code, stdout, stderr = project.run_status("forget", "--match", "bicycle")
	assert code == 0, f"the guarded sweep failed: out={stdout} err={stderr}"
	assert "removed nothing" in stdout, f"a kept fact must not read as a removal: {stdout}"
	found = [h.text for h in hits(project.run("query", "--mode", "vector", BIKE)[0])]
	assert any("bicycle" in t for t in found), f"the guard let a fact through: {found}"

	code, stdout, stderr = project.run_status("forget", "--match", "bicycle", "--force")
	assert code == 0, f"the sweep failed: out={stdout} err={stderr}"
	assert stdout.startswith("forgot "), f"the sweep says what it removed: {stdout}"
	found = [h.text for h in hits(project.run("query", "--mode", "vector", BIKE)[0])]
	assert not any("bicycle" in t for t in found), f"the sweep removed nothing: {found}"


def test_a_flag_the_id_path_would_ignore_is_refused_not_swallowed(project):
	ingest_all(project, [BIKE])
	bike_id = full_id(project, BIKE)

	code, stdout, stderr = project.run_status("forget", "--force", bike_id)
	assert code != 0, f"a silently ignored flag is worse than none: out={stdout}"
	assert "--force" in stderr, f"the refusal names the flag: {stderr}"
	# And it really did not remove it.
	code, _, _ = project.run_status("get", bike_id)
	assert code == 0, "the refused forget left the thought alone"


def test_forget_with_no_target_is_a_usage_error(project):
	code, stdout, stderr = project.run_status("forget")
	assert code != 0, f"a no-op that exits 0 reads as a removal: out={stdout}"
	assert "--source" in stderr and "--match" in stderr, (
		f"the error says what a target looks like: {stderr}"
	)
