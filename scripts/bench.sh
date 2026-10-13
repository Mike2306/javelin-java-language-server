#!/usr/bin/env bash
# Run the Criterion benchmarks.
#
# Criterion stores results in target/criterion and compares each run against the
# previous one, so the useful workflow is: run on the base revision, switch, run
# again, read the change report.
#
#   scripts/bench.sh                 # everything
#   scripts/bench.sh parsing         # one bench target
#   scripts/bench.sh -- --quick      # pass flags through to Criterion
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $# -gt 0 && $1 != --* ]]; then
  target="$1"; shift
  exec cargo bench -p javelin-benchmarks --bench "$target" "$@"
fi
exec cargo bench -p javelin-benchmarks "$@"
