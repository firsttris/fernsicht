#!/bin/sh
# Builds everything an installation needs (release): the host, the client
# with its window, the app with its UI. Runs in the distrobox; started on
# the host, it enters the box by itself.
#
#   ./packaging/build.sh
set -eu

cd "$(dirname "$0")/.."
if [ ! -e /run/.containerenv ] && command -v distrobox >/dev/null 2>&1; then
    exec distrobox enter fernsicht -- sh -c "cd '$PWD' && ./packaging/build.sh"
fi

bun install --frozen-lockfile
bun run --filter @fernsicht/client-ui build
bun run --filter @fernsicht/viewer build
cargo build --release -p fernsicht-host-agent --features vaapi,kms,nvidia,vulkan
cargo build --release -p fernsicht-client --features vaapi,nvidia,vulkan,window
cargo build --release --manifest-path apps/desktop/Cargo.toml

echo
echo "Fertig. Installieren (auf dem Host, nicht in der Box):"
echo "  ./packaging/install-app.sh          App und Client, für deinen Benutzer"
echo "  sudo ./packaging/install-host.sh    Host-Dienst, auf Rechnern, die man fernsteuern will"
