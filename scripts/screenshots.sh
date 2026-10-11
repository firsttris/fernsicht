#!/usr/bin/env bash
# Takes the pictures in docs/ (README, documentation, social preview) from the
# demo data of the app UI and the web viewer: pnpm screenshots
# Builds both, serves them with vite preview, runs web/e2e/screenshots.mjs and
# stops the servers again. The workflow "Update screenshots" runs this in the
# Playwright image and commits the pictures that changed.
set -euo pipefail
cd "$(dirname "$0")/.."
pnpm --filter @fernsicht/client-ui build
pnpm --filter @fernsicht/viewer build
# The servers themselves (exec), not a pnpm wrapper: so they end with the trap.
(cd apps/client-ui && exec node_modules/.bin/vite preview --port 4311 --strictPort >/dev/null) &
client=$!
(cd web/viewer && exec node_modules/.bin/vite preview --port 4312 --strictPort >/dev/null) &
viewer=$!
trap 'kill $client $viewer 2>/dev/null || true' EXIT
for url in http://localhost:4311 http://localhost:4312; do
  for i in $(seq 60); do
    node -e "fetch('$url').then(r => process.exit(r.ok ? 0 : 1), () => process.exit(1))" && break
    (( i == 60 )) && { echo "$url did not start" >&2; exit 1; }
    sleep 1
  done
done
pnpm --filter @fernsicht/web-e2e exec node screenshots.mjs "$PWD/docs"
