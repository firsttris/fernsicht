#!/usr/bin/env bash
# Everything CI checks that runs on a machine without a GPU, in one go:
# green here before every push (CLAUDE.md). Stops at the first failure.
# The shell scripts and the docs are checked when the tools are installed
# (or given as SHELLCHECK and MKDOCS); --quick leaves out the end-to-end and
# browser tests.
set -euo pipefail
cd "$(dirname "$0")/.."
quick=false
[[ "${1:-}" == "--quick" ]] && quick=true

step() { printf '\n── %s\n' "$*"; }

step "Rust: format and clippy"
cargo fmt --all --check
cargo fmt --manifest-path fuzz/Cargo.toml --check
cargo clippy --workspace --all-targets -- -D warnings

step "Rust: tests"
if $quick; then
    cargo test --workspace --exclude fernsicht-e2e
else
    cargo test --workspace
fi
PROPTEST_CASES=1024 cargo test --workspace --test properties

step "Web: format, types, tests, build"
pnpm format:check
pnpm -r typecheck
pnpm test
pnpm -r build
if ! $quick; then
    FERNSICHT_UI_BUILT=1 pnpm test:e2e
fi

step "Desktop app: format, clippy, tests (and the generated TypeScript)"
(
    cd apps/desktop
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test
)

shellcheck=${SHELLCHECK:-$(command -v shellcheck || true)}
if [[ -n "$shellcheck" ]]; then
    step "Shell scripts"
    "$shellcheck" packaging/*.sh scripts/*.sh dev/*.sh dev/runner/*.sh
fi
mkdocs=${MKDOCS:-$(command -v mkdocs || true)}
if [[ -n "$mkdocs" ]]; then
    step "Documentation"
    "$mkdocs" build --strict --quiet --site-dir "$(mktemp -d)"
fi
step "All green"
