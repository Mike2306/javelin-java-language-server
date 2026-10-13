#!/usr/bin/env bash
# The full local gate: CI's steps in CI's order, plus the recording step CI does
# not run, minus `cargo bench -p javelin-benchmarks --no-run`, which CI does.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

step "format"
cargo fmt --all -- --check

step "clippy"
cargo clippy --workspace --all-targets -- -D warnings

step "build"
cargo build --workspace --all-targets

step "test"
cargo test --workspace --all-targets

# The one thing here that writes to the tree. A modified
# docs/editor-pass-results.md after this step is the signal the numbers moved.
#
# `#[ignore]`d in the suite because it reads the machine's own JDK, so this is
# the only place it runs. A filter that matches nothing exits 0, which would
# leave the gate green and the doc silently stale after a rename, so the pass
# count is checked rather than assumed.
#
# Captured rather than piped through `tee /dev/stderr`: on Linux that path
# resolves through `/proc/self/fd/2`, so `./scripts/check.sh 2> err.log` makes
# `tee` re-open the log with O_TRUNC and wipe what the earlier steps wrote.
# Capturing costs the live stream, so the failure branch is explicit: `set -e`
# would otherwise end the script *at the assignment*, and the panic that ended
# the test would never be printed.
step "record the editor pass numbers"
recorded=$(JAVELIN_RECORD=1 cargo test -p javelin --test editor_pass \
    the_measured_numbers_are_recorded -- --ignored --exact 2>&1) || {
    printf '%s\n' "$recorded"
    exit 1
}
printf '%s\n' "$recorded"
grep '^test result: ok\. 1 passed' <<<"$recorded" >/dev/null

step "docs"
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

printf '\n\033[1;32mall checks passed\033[0m\n'
