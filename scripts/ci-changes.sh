#!/usr/bin/env bash
# Which parts a push or pull request touches, as "rust=…", "web=…" and
# "gpu=…" lines for $GITHUB_OUTPUT: CI then skips the jobs of the other part,
# and the GPU workflow its runners unless GPU code changed. A schedule, a
# release, a new branch or an unknown base checks everything.
# Env: EVENT (github.event_name), BEFORE (a push's previous commit), BASE (a
# pull request's base commit).
set -euo pipefail

case "${EVENT:-}" in
    push) base=${BEFORE:-} ;;
    pull_request) base=${BASE:-} ;;
    *) base= ;;
esac
if [[ -z "$base" || "$base" =~ ^0+$ ]] || ! git cat-file -e "$base^{commit}" 2>/dev/null; then
    echo "rust=true"
    echo "web=true"
    echo "gpu=true"
    exit 0
fi
files=$(git diff --name-only "$base" HEAD)

# The workflows and the scripts they run concern both parts. The desktop
# app and the generated TypeScript types sit between them.
shared='^(\.github/|scripts/)'
rust='^(Cargo\.(toml|lock)$|rust-toolchain\.toml$|crates/|apps/(client|host-agent|desktop)/|tests/|fuzz/|packaging/|dev/)'
web='^(web/|apps/desktop/|package\.json$|pnpm-(lock|workspace)\.yaml$|biome\.json$)'
# What the GPU runners test: the GPU crates, the host's encoder and the
# client's decoder and window, the GPU tests and what builds them. Network,
# protocol, input, sound and the web viewer are tested in CI already.
gpu='^(Cargo\.(toml|lock)$|crates/(capture|codec|core|gpu|render|shader)/|apps/(client|host-agent)/Cargo\.toml$|apps/host-agent/src/(backends|encode|power)\.rs$|apps/client/src/present\.rs$|tests/e2e/tests/(gpu|web_gpu)\.rs$|dev/|\.github/workflows/gpu\.yml$|scripts/(annotate-failures|ci-changes)\.sh$)'
touches() { grep -qE "$1|$shared" <<<"$files"; }
echo "rust=$(touches "$rust" && echo true || echo false)"
echo "web=$(touches "$web" && echo true || echo false)"
echo "gpu=$(grep -qE "$gpu" <<<"$files" && echo true || echo false)"
