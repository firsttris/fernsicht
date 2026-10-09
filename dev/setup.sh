#!/usr/bin/env bash
# Builds the dev image and creates the distrobox. Run on the host.
set -euo pipefail
cd "$(dirname "$0")"
podman build -t localhost/fernsicht-dev:latest -f Containerfile .
distrobox assemble create --file distrobox.ini
cat <<'MSG'

Fertig. Weiter mit:
  distrobox enter fernsicht
  cargo test --workspace

Für KMS-Capture (Phase 1) braucht das Host-Binary CAP_SYS_ADMIN:
  sudo setcap cap_sys_admin+p target/release/fernsicht-host-agent

Größere UDP-Puffer gegen Drops bei Keyframes (auf dem Host-System):
  sudo sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608
MSG
