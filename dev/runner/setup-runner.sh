#!/usr/bin/env bash
# Sets up (or updates) the self-hosted GitHub Actions runner for GPU tests
# on a Bazzite machine. See docs/gpu-runner.md.
#
#   dev/runner/setup-runner.sh --gpu amd       AMD (/dev/dri)
#   dev/runner/setup-runner.sh --gpu nvidia    NVIDIA (CDI)
#   dev/runner/setup-runner.sh --gpu amd --remove
#
# Options: --token TOKEN (otherwise asked for), --repo OWNER/NAME,
#          --name RUNNER_NAME. Run again after Bazzite updates to rebuild.
set -euo pipefail

repo="firsttris/fernsicht"
gpu=""
token="${RUNNER_TOKEN:-}"
name=""
remove=0
while [ $# -gt 0 ]; do
  case "$1" in
    --gpu) gpu="${2:-}"; shift 2 ;;
    --token) token="${2:-}"; shift 2 ;;
    --repo) repo="${2:-}"; shift 2 ;;
    --name) name="${2:-}"; shift 2 ;;
    --remove) remove=1; shift ;;
    -h | --help) sed -n '2,11p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$gpu" in
  amd) device="/dev/dri" ;;
  nvidia) device="nvidia.com/gpu=all" ;;
  *) echo "usage: $0 --gpu amd|nvidia [--token TOKEN] [--remove]" >&2; exit 2 ;;
esac

here="$(cd "$(dirname "$0")" && pwd)"
unit="fernsicht-runner-${gpu}"
quadlet_dir="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd"
volume="fernsicht-runner-${gpu}"
name="${name:-fernsicht-$(hostname -s)-${gpu}}"

if [ "$remove" = 1 ]; then
  systemctl --user stop "${unit}.service" 2>/dev/null || true
  rm -f "${quadlet_dir}/${unit}.container"
  systemctl --user daemon-reload
  podman volume rm -f "$volume" >/dev/null 2>&1 || true
  echo "Lokal entfernt. Den Runner zusätzlich auf GitHub löschen:"
  echo "  https://github.com/${repo}/settings/actions/runners"
  exit 0
fi

command -v podman >/dev/null || { echo "podman fehlt" >&2; exit 1; }

# --- Preflight ----------------------------------------------------------------
if [ "$gpu" = amd ]; then
  render=""
  for node in /dev/dri/renderD*; do
    [ -e "$node" ] && { render="$node"; break; }
  done
  [ -n "$render" ] || { echo "Kein /dev/dri/renderD*: AMD-Treiber geladen?" >&2; exit 1; }
  if [ ! -r "$render" ] || [ ! -w "$render" ]; then
    echo "Kein Zugriff auf $render. Siehe docs/gpu-runner.md → Fehlerbehebung (render-Gruppe)." >&2
    exit 1
  fi
else
  if ! nvidia-smi >/dev/null 2>&1; then
    echo "nvidia-smi geht nicht: NVIDIA-Image von Bazzite (geschlossener Treiber) installiert?" >&2
    exit 1
  fi
  if ! nvidia-ctk cdi list 2>/dev/null | grep -q 'nvidia.com/gpu=all'; then
    echo "Keine CDI-Spezifikation für die GPU. Einmalig erzeugen:" >&2
    echo "  sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml" >&2
    exit 1
  fi
fi

# --- Images -------------------------------------------------------------------
fedora="$(rpm -E %fedora 2>/dev/null || echo 44)"
echo "==> Baue Images (Fedora ${fedora}) ..."
podman build --build-arg "FEDORA_VERSION=${fedora}" -t localhost/fernsicht-dev:latest -f "${here}/../Containerfile" "${here}/.."
podman build -t localhost/fernsicht-runner:latest -f "${here}/Containerfile" "$here"

# Helper containers use the same SELinux setting as the runner service
# (label=disable); otherwise they could not read the volume the service
# has been writing to.
run_helper() {
  podman run --rm --security-opt=label=disable -v "${volume}:/runner" "$@"
}

# --- Registration (once) --------------------------------------------------------
podman volume create --ignore "$volume" >/dev/null
state="$(run_helper --entrypoint sh localhost/fernsicht-runner:latest -c \
  'if [ -f /runner/.runner ]; then echo registered; elif [ -r /runner ] && [ -x /runner ]; then echo new; else echo denied; fi')"
if [ "$state" = denied ]; then
  echo "Kein Zugriff auf das Volume ${volume}. Siehe docs/gpu-runner.md → Fehlerbehebung." >&2
  exit 1
elif [ "$state" = registered ]; then
  echo "==> Runner ist bereits registriert."
else
  if [ -z "$token" ]; then
    echo "Registrierungs-Token von https://github.com/${repo}/settings/actions/runners/new"
    read -rsp "Token: " token
    echo
  fi
  echo "==> Registriere ${name} (Labels: self-hosted, linux, gpu-${gpu}) ..."
  # The token goes in via the environment, never onto a command line.
  RUNNER_TOKEN="$token" run_helper \
    --env RUNNER_TOKEN \
    -e "RUNNER_URL=https://github.com/${repo}" \
    -e "RUNNER_NAME=${name}" \
    -e "RUNNER_LABELS=self-hosted,linux,gpu-${gpu}" \
    localhost/fernsicht-runner:latest register
fi

# --- systemd user service via Quadlet ----------------------------------------
mkdir -p "$quadlet_dir"
sed -e "s|@GPU@|${gpu}|g" -e "s|@DEVICE@|${device}|g" \
  "${here}/fernsicht-runner.container.in" >"${quadlet_dir}/${unit}.container"
# Keep the runner alive without an open login session.
loginctl enable-linger "$USER"
systemctl --user daemon-reload
systemctl --user restart "${unit}.service"

echo "==> GPU-Check im Runner-Container:"
podman run --rm --device "$device" --group-add=keep-groups --security-opt=label=disable \
  -v "${here}/../gpu-check.sh:/gpu-check.sh:ro" --entrypoint bash \
  localhost/fernsicht-runner:latest /gpu-check.sh --expect "$gpu" || {
  echo "GPU-Check fehlgeschlagen, siehe docs/gpu-runner.md → Fehlerbehebung." >&2
  exit 1
}

cat <<MSG

Fertig. Der Runner läuft als systemd-User-Dienst ${unit}.
  Status:  systemctl --user status ${unit}
  Logs:    journalctl --user -u ${unit} -f
  GitHub:  https://github.com/${repo}/settings/actions/runners  (sollte "Idle" zeigen)
  Testen:  https://github.com/${repo}/actions/workflows/gpu.yml → "Run workflow"
MSG
