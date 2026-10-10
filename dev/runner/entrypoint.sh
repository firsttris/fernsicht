#!/usr/bin/env bash
# Runner container entry point.
#   register   one-time registration (needs RUNNER_TOKEN, RUNNER_URL,
#              RUNNER_NAME, RUNNER_LABELS), then exit
#   run        start the runner (default)
# Runner state (registration, work dir, self-updates) lives in /runner, a
# named volume, so it survives image rebuilds.
set -euo pipefail

mkdir -p /runner /cache
cd /runner
[ -x ./run.sh ] || cp -a /opt/actions-runner/. /runner/

case "${1:-run}" in
  register)
    : "${RUNNER_TOKEN:?RUNNER_TOKEN fehlt}"
    ./config.sh --unattended --replace \
      --url "${RUNNER_URL:?}" \
      --token "$RUNNER_TOKEN" \
      --name "${RUNNER_NAME:?}" \
      --labels "${RUNNER_LABELS:?}" \
      --work /runner/_work
    ;;
  run)
    if [ ! -f .runner ]; then
      echo "Runner ist nicht registriert: dev/runner/setup-runner.sh ausführen." >&2
      exit 1
    fi
    exec ./run.sh
    ;;
  *)
    exec "$@"
    ;;
esac
