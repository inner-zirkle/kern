#!/usr/bin/env bash
# m2-fold-put.sh — kern's graph+store+dedup leg vs the model repo's in-process put.
#
# The number `docs/specs/25-kern-fold.md` §5 (model repo) left unmeasured. §5
# settled the embed leg negatively — it is HTTP by architecture, so a fold
# cannot remove that round-trip — but said of the rest only that the cost
# "softens", with no figure. This script produces the figure.
#
# Both sides are re-measured here, in one run on one box, rather than quoting
# VISION.md's remembered "~1 µs": a ratio built from two numbers taken on
# different machines on different days is not a ratio.
#
# Usage: docs/lab/bench/m2-fold-put.sh [n_puts] [n_prepop] [dim] [n_record]
set -euo pipefail

N_PUTS=${1:-1000}
N_PREPOP=${2:-20000}
DIM=${3:-1024}
N_RECORD=${4:-100000}
BUDGET=10   # the "under 10x record's put" gate this ticket tests

KERN_ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
MODEL_ROOT=${MODEL_ROOT:-$HOME/dev/llm}

if [ ! -d "$MODEL_ROOT/src/record" ]; then
  echo "SKIP: model repo not at $MODEL_ROOT (set MODEL_ROOT) — no baseline to compare against" >&2
  exit 3
fi

echo "building both sides…" >&2
( cd "$KERN_ROOT"  && cargo build --release -p ingest --bin m2_fold_put >/dev/null 2>&1 )
( cd "$MODEL_ROOT" && cargo build --release -p llm-record --bin bench_put >/dev/null 2>&1 )

K=$("$KERN_ROOT/target/release/m2_fold_put" "$N_PUTS" "$N_PREPOP" "$DIM" 2>/dev/null)
DEDUP=$(echo "$K" | awk '/kern_dedup_us/{print $2}')
INSERT=$(echo "$K" | awk '/kern_insert_us/{print $2}')
LEG=$(echo "$K"    | awk '/kern_leg_us/{print $2}')

# Best-of-3 for the baseline: it is sub-microsecond, so scheduler noise is a
# large fraction of it, and the minimum is the least-contaminated estimate.
BASE=$(for _ in 1 2 3; do "$MODEL_ROOT/target/release/bench_put" "$N_RECORD" | awk '/record_put_us/{print $2}'; done | sort -g | head -1)

RATIO=$(awk -v l="$LEG" -v b="$BASE" 'BEGIN{printf "%.0f", l/b}')
VERDICT=$(awk -v r="$RATIO" -v g="$BUDGET" 'BEGIN{print (r<=g)?"MET":"MISSED"}')

printf '\n%-14s %12s\n' "leg" "µs/put"
printf '%-14s %12s\n'   "--------------" "------------"
printf '%-14s %12.3f\n' "record put"   "$BASE"
printf '%-14s %12.3f\n' "graph insert" "$INSERT"
printf '%-14s %12.3f\n' "dedup scan"   "$DEDUP"
printf '%-14s %12.3f\n' "kern leg"     "$LEG"
printf '\nn=%s puts, %s resident, dim=%s, record n=%s, box=%s\n' \
  "$N_PUTS" "$N_PREPOP" "$DIM" "$N_RECORD" "$(uname -sm)"
printf 'load at run: %s\n' "$(uptime | sed 's/.*load average: //')"
printf '\nstore leg: %s µs (graph %s / dedup %s), record baseline %s µs, ratio %s× — budget %s× %s\n' \
  "$LEG" "$INSERT" "$DEDUP" "$BASE" "$RATIO" "$BUDGET" "$VERDICT"
