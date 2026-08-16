set windows-shell := ["pwsh", "-NoLogo", "-NoProfile", "-Command"]

# Shared debug/test image — sibling repo: https://github.com/yesitsfebreeze/rustest
rustest_repo := "https://github.com/yesitsfebreeze/rustest"
rustest_dir  := justfile_directory() / ".." / "rustest"
image        := "rustest:latest"

# shows this help
help:
    @just --list

# all static checks: formatting + clippy with warnings as errors
check:
    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings

# debug build
build:
    cargo build

# Release build
release:
    cargo build --release

# launch the mux: agent pane + cwd daemon + kern MCP, one process
run:
    cargo run --bin kern

# headless daemon only (no TUI/panes) — servers or background use
daemon:
    cargo run --bin kern -- daemon

# full test suite: nextest + doc tests + e2e pytest
test:
    cargo nextest run --workspace
    cargo test --doc --workspace
    pytest -q -s tests/e2e

# E2E harness alone: real binary against a deterministic fake LLM.
# -s so the recall metric prints on a GREEN run too — a number only visible when
# it trips is a number nobody watches drift toward the floor.
e2e:
    pytest -q -s tests/e2e

# apply formatting
fmt:
    cargo fmt --all

# install the release binary via cargo
install: release
    cargo install --path . --force

# remove the installed binary
uninstall:
    cargo uninstall kern

# wipe build output and local runtime state
[unix]
clean:
    cargo clean
    rm -rf .relay .mesh .git-fs .machine
    rm -rf .kern/bin .kern/intake .kern/data .kern/*.log
    rm -rf traces docs/site/out docs/site/.next

# wipe build output and local runtime state
[windows]
clean:
    cargo clean
    -Remove-Item -Recurse -Force .relay, .mesh, .git-fs, .machine
    -Remove-Item -Recurse -Force ".kern\bin", ".kern\intake", ".kern\data"
    -Remove-Item -Force ".kern\*.log"
    -Remove-Item -Recurse -Force "traces", "docs\site\out", "docs\site\.next"

# kill running kern processes
[windows]
kill:
    -taskkill /IM kern.exe /F 2>$null

# kill running kern processes
[unix]
kill:
    -pkill -f kern

# serve the docs site locally (dev server)
docs:
    cd docs/site && npm run dev

# static-export the docs site to docs/site/out
docs-build:
    cd docs/site && npm run build

# download the benchmark datasets into tests/eval/ (gitignored; LoCoMo is
# CC BY-NC, BEAM is CC BY-SA). `just eval-fetch beam --beam-scales 500K,1M`
# for the bigger BEAM splits.
eval-fetch *args:
    python3 tests/e2e/eval/datasets.py {{args}}

# LoCoMo-10 retrieval-only benchmark against a local real embedder (Ollama).
# Slow and user-run by design — CI only runs the scorer unit tests.
eval-locomo *args:
    python3 tests/e2e/eval/run_locomo.py {{args}}

# LongMemEval-S retrieval-only benchmark; seeded 100-question sample by
# default, `--full` for all 500 (hours of embedding).
eval-longmemeval *args:
    python3 tests/e2e/eval/run_longmemeval.py {{args}}

# kern's own committed ground-truth corpus (tests/e2e/eval/ground.json);
# `--path direct` is the verbatim floor, `--path distill` runs the real
# intake pipeline and scores claims by their cited-turn provenance.
eval-ground *args:
    python3 tests/e2e/eval/run_ground.py {{args}}

# BEAM end-to-end benchmark: kern retrieval feeds an answer LLM, an LLM
# judge scores against the dataset rubric, per-ability report (IE..SUM).
# `--mode retrieval` is the no-LLM coverage proxy. Needs `just eval-fetch beam`.
eval-beam *args:
    python3 tests/e2e/eval/run_beam.py {{args}}

# install e2e harness dependencies. Plain install first: --break-system-packages
# is unknown to pip < 23 and would turn a working environment into a hard error,
# so it is the fallback for a PEP 668 distro python, not the default.
e2e-install:
    pip install -r tests/e2e/requirements.txt || pip install --break-system-packages -r tests/e2e/requirements.txt

# install docs site dependencies
docs-install:
    cd docs/site && npm ci

# point git at the tracked oracle pre-commit hook (idempotent)
hooks:
    git config core.hooksPath hooks

# verify every src/...:line citation and page link in the docs site is alive
docs-check:
    python3 tests/docs_check.py --selftest
    python3 tests/docs_check.py

# build the shared rustest image (clone the sibling repo if missing)
[unix]
docker-build:
    test -d "{{rustest_dir}}" || git clone {{rustest_repo}} "{{rustest_dir}}"
    cd "{{rustest_dir}}" && just build

# build the shared rustest image (clone the sibling repo if missing)
[windows]
docker-build:
    if (-not (Test-Path "{{rustest_dir}}")) { git clone {{rustest_repo}} "{{rustest_dir}}" }
    cd "{{rustest_dir}}"; just build

# interactive shell in the debug/test container (repo mounted at /work)
docker:
    docker run --rm -it --cap-add SYS_PTRACE --security-opt seccomp=unconfined -v "{{justfile_directory()}}":/work -w /work {{image}} /bin/bash

# full test suite inside the container (installs nextest if missing)
docker-test:
    docker run --rm --cap-add SYS_PTRACE --security-opt seccomp=unconfined -v "{{justfile_directory()}}":/work -w /work {{image}} sh -c 'command -v cargo-nextest >/dev/null || curl -LsSf https://get.nexte.st/latest/linux | tar zxf - -C /usr/local/bin; cargo nextest run --workspace && cargo test --doc --workspace'
