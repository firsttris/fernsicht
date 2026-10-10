#!/usr/bin/env bash
# Builds the dev image and creates the Distrobox. Run on the host.
#   dev/setup.sh            AMD/Intel
#   dev/setup.sh --nvidia   NVIDIA
set -euo pipefail
cd "$(dirname "$0")"

box=fernsicht
case "${1:-}" in
  --nvidia) box=fernsicht-nvidia ;;
  "") ;;
  *) echo "usage: $0 [--nvidia]" >&2; exit 2 ;;
esac

fedora="$(rpm -E %fedora 2>/dev/null || echo 44)"
podman build --build-arg "FEDORA_VERSION=${fedora}" -t localhost/fernsicht-dev:latest -f Containerfile .
distrobox assemble create --file distrobox.ini --name "$box" --replace

cat <<MSG

Fertig. Weiter mit:
  distrobox enter ${box}
  dev/gpu-check.sh          # was kann die GPU, laufen Hardware-Encode/-Decode?
  cargo test --workspace

Größere UDP-Puffer gegen Drops bei Keyframes (auf dem Host-System):
  sudo sysctl -w net.core.rmem_max=8388608 net.core.wmem_max=8388608
MSG
