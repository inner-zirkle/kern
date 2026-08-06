#!/usr/bin/env bash
set -euo pipefail

# Run all prevention checks
DIR="$(cd "$(dirname "$0")" && pwd)"
fail=0

for check in "$DIR/checks/"*.sh; do
  [ -f "$check" ] || continue
  if ! bash "$check"; then
    echo "FAIL: $(basename "$check")"
    fail=1
  fi
done

exit $fail
