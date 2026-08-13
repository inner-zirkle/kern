#!/usr/bin/env bash
set -euo pipefail
just docs-install
just e2e-install
just hooks
