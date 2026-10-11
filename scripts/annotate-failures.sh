#!/usr/bin/env bash
# Runs a test command; when it fails, names the failed tests and their
# panics as GitHub annotations. The run's page and the API show those
# without a login, job logs only with one.
#
#   scripts/annotate-failures.sh cargo test --workspace
set -uo pipefail

log=$(mktemp)
"$@" 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
if [ "$status" -ne 0 ]; then
  awk '/^test .* FAILED$/ || /^minimal failing input/ { print; next }
       /panicked at/ { line = $0; if ((getline next_line) > 0) line = line " " next_line; print line }' "$log" |
    head -n 20 |
    sed -e 's/%/%25/g' -e 's/^/::error title=Test failure::/'
fi
rm -f "$log"
exit "$status"
