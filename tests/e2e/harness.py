"""KernProject — one isolated kern install driven through its own binary.

Lifted out of conftest.py so non-pytest callers (scripts/eval/) can drive the
same harness; conftest re-exports everything here, so tests keep importing
from conftest unchanged.
"""

import json
import os
import subprocess
import time
from pathlib import Path


class KernProject:
	"""One isolated kern project: private cwd, runtime dir, config dir.

	XDG_RUNTIME_DIR isolates the hub socket from any user hub;
	XDG_CONFIG_HOME keeps the user's real kern.toml out of the layered load.
	"""

	def __init__(self, kern_bin, tmp_path, llm_url):
		self.bin = str(kern_bin)
		self.cwd = tmp_path / "proj"
		# A short runtime dir, not tmp_path/run: the daemon socket lives under
		# XDG_RUNTIME_DIR, and a deep pytest tmpdir would exceed SUN_LEN and kill
		# the bind (kern falls back to /tmp, which this scan would then miss).
		self.runtime = Path("/tmp") / f"kern-test-{os.getpid()}-{time.time_ns()}"
		self.llm_url = llm_url
		config_home = tmp_path / "config"
		for d in (self.cwd / ".kern", self.runtime, config_home):
			d.mkdir(parents=True)
		self.write_config()
		self.env = os.environ | {
			"XDG_RUNTIME_DIR": str(self.runtime),
			"XDG_CONFIG_HOME": str(config_home),
		}
		self._children = []

	def write_config(
		self,
		data_dir=None,
		intake_enabled=True,
		intake_retention_secs=0,
		review_policy=None,
		watcher_enabled=False,
		watcher_roots=None,
		gnn_min_thoughts=None,
		tick_interval_secs=None,
		reason_timeout_secs=None,
		embed=None,
		reason=None,
		max_deliver_results=None,
	):
		"""(Re)write the project kern.toml. `data_dir` is cwd-relative.

		Config is read once per process, at startup — so rewriting this while a
		daemon runs repoints the *next* CLI invocation without moving the store
		the daemon already holds open.

		`intake_enabled=False` stops the daemon spawning its own intake poll
		loop; `kern intake drain` ignores the flag, being an explicit request.

		`intake_retention_secs` is the standing per-source TTL for everything
		that queue ingests. It is omitted when 0 so the default config text —
		what every other test loads — stays exactly what it was.

		`review_policy` is a `{scheme: state}` mapping — e.g.
		`{"inline": "pending"}` — written into `[ingest]`, which holds every
		ingest on that scheme out of a `query --exclude-pending` until `kern
		promote` releases it. Omitted when None, for the same byte-identity
		reason as the TTL above.

		`watcher_enabled` / `watcher_roots` turn on the file watcher, whose
		`roots` are cwd-relative and default to the whole cwd. The section is
		emitted only when enabled, for the same reason: every other test's
		config text stays byte-identical.

		`gnn_min_thoughts` lowers the GNN's entity floor and `tick_interval_secs`
		sets the maintenance driver's period — `0` disables the periodic driver,
		leaving only the one maintenance pass a daemon enqueues at boot. Both are
		omitted when None, again so every other test's config text is unchanged.

		`reason_timeout_secs` is the ceiling on one completion. Omitted when None
		for the same byte-identity reason — and that omission is the default under
		test: every other e2e runs with no such key and posts under the 600s the
		`const` used to hardcode.

		`embed` is an `(url, model)` pair pointing at a real embedding endpoint —
		the eval harness's knob; None keeps the fake. `reason` is the same pair
		for the completion endpoint — the distill-path eval's knob; None keeps
		the fake. `max_deliver_results` widens the delivered list past the
		preset for recall@k with k above the cap. All omitted when None, same
		byte-identity reason as everything above.
		"""
		head = f'data_dir = "{data_dir}"\n\n' if data_dir else ""
		ttl = (
			f"retention_secs = {intake_retention_secs}\n" if intake_retention_secs else ""
		)
		review = ""
		if review_policy:
			pairs = ", ".join(f'{k} = "{v}"' for k, v in review_policy.items())
			review = f"[ingest]\nreview_policy = {{ {pairs} }}\n\n"

		watcher = ""
		if watcher_enabled:
			roots = f"roots = {json.dumps(list(watcher_roots))}\n" if watcher_roots else ""
			watcher = f"\n[watcher]\nenabled = true\n{roots}"

		gnn = f"\n[gnn]\nmin_thoughts = {gnn_min_thoughts}\n" if gnn_min_thoughts else ""
		tick = (
			f"\n[tick]\ninterval_secs = {tick_interval_secs}\n"
			if tick_interval_secs is not None
			else ""
		)
		reason_timeout = (
			f"timeout_secs = {reason_timeout_secs}\n" if reason_timeout_secs else ""
		)
		embed_url, embed_model = embed if embed else (self.llm_url, "fake-embed")
		reason_url, reason_model = reason if reason else (self.llm_url, "fake-reason")
		# KERN_EVAL_RETRIEVAL_TOML injects extra `[retrieval]` keys (e.g.
		# `lexical_top_boost = 1.0`) for ablation runs. Unset => empty, so every
		# other test's config text stays byte-identical.
		retrieval_extra = os.environ.get("KERN_EVAL_RETRIEVAL_TOML", "")
		if max_deliver_results is not None or retrieval_extra:
			retrieval = f"\n[retrieval]\n"
			if max_deliver_results is not None:
				retrieval += f"max_deliver_results = {max_deliver_results}\n"
			if retrieval_extra:
				retrieval += retrieval_extra.strip() + "\n"
		else:
			retrieval = ""
		(self.cwd / ".kern" / "kern.toml").write_text(
			f"{head}"
			f'[embed]\nurl = "{embed_url}"\nmodel = "{embed_model}"\n\n'
			f'[reason]\nurl = "{reason_url}"\nmodel = "{reason_model}"\n'
			f"{reason_timeout}\n"
			f"{review}"
			f"[intake]\nenabled = {str(intake_enabled).lower()}\n{ttl}"
			f"{watcher}{gnn}{tick}{retrieval}\n"
		)

	def run(self, *args, timeout=120):
		out = subprocess.run(
			[self.bin, *args],
			cwd=self.cwd,
			env=self.env,
			capture_output=True,
			text=True,
			timeout=timeout,
		)
		return out.stdout, out.stderr

	def spawn(self, *args, **popen_kw):
		popen_kw.setdefault("stdin", subprocess.DEVNULL)
		popen_kw.setdefault("stdout", subprocess.DEVNULL)
		popen_kw.setdefault("stderr", subprocess.DEVNULL)
		return subprocess.Popen(
			[self.bin, *args], cwd=self.cwd, env=self.env, **popen_kw
		)

	def start_hub(self, *extra):
		child = self.spawn("hub", *extra)
		self._children.append(child)
		sock = self.runtime / "kern-hub.sock"
		wait_until(lambda: sock.exists(), 10, f"hub never bound {sock}")
		return child

	def node_sockets(self):
		"""Every per-project daemon socket in this project's private runtime dir.

		The name carries an FNV tag of the daemon's cwd (trnsprt::typed::local),
		which the test has no way to recompute — so it matches on the shape.
		"""
		return [
			p
			for p in self.runtime.iterdir()
			if p.name.startswith("kern-") and p.name != "kern-hub.sock"
		]

	def start_daemon(self, **popen_kw):
		child = self.spawn("--daemon", **popen_kw)
		self._children.append(child)
		wait_until(
			lambda: bool(self.node_sockets()),
			60,
			f"daemon never bound a socket under {self.runtime}",
		)
		return child

	def stop(self, child):
		"""Kill a child and reap it.

		A killed daemon leaves its socket *file* behind — nothing unlinks it. That
		is exactly the shape the NoDaemon fallback has to survive in the field, so
		the test does not clean it up: connect-refused must read as "no daemon".
		"""
		child.kill()
		child.wait()
		if child in self._children:
			self._children.remove(child)

	def kill_all(self):
		for child in self._children:
			child.kill()
			child.wait()


def wait_until(cond, secs, msg):
	deadline = time.monotonic() + secs
	while not cond():
		assert time.monotonic() < deadline, msg
		time.sleep(0.1)
