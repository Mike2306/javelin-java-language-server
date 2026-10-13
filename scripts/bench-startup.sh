#!/usr/bin/env bash
# Measure cold process startup: fork, exec, dynamic linking, and everything
# before the first byte of LSP traffic.
#
# This cannot be measured from inside the process, so it is a script rather than
# a Criterion benchmark. Budget: under 50 ms.
#
# Uses hyperfine when available (better statistics, warm/cold separation) and
# falls back to a plain loop otherwise.
set -euo pipefail
cd "$(dirname "$0")/.."

binary="target/release/javelin"
[[ -x $binary ]] || { echo "building release binary first"; cargo build --release -p javelin; }

# `--version` exits immediately after startup, so it measures process start and
# nothing else.
if command -v hyperfine >/dev/null 2>&1; then
  exec hyperfine --warmup 10 --runs 200 "$binary --version"
fi

echo "hyperfine not found; using a plain loop (install hyperfine for percentiles)"
runs=100
start=$(date +%s%N)
for _ in $(seq $runs); do "$binary" --version >/dev/null; done
end=$(date +%s%N)
echo "mean startup: $(( (end - start) / runs / 1000000 )) ms over $runs runs"
