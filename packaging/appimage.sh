#!/bin/sh
# Builds Fernsicht.AppImage: the app, the stream window (fernsicht-client),
# the host (fernsicht-host-agent) and the web viewer in one file. The app
# installs the host from it as a system service ("Diesen Rechner freigeben").
# Runs in the distrobox; started on the host, it enters the box by itself.
#
#   ./packaging/appimage.sh    → apps/desktop/target/release/bundle/appimage/
set -eu

cd "$(dirname "$0")/.."
if [ ! -e /run/.containerenv ] && command -v distrobox >/dev/null 2>&1; then
    exec distrobox enter fernsicht -- sh -c "cd '$PWD' && ./packaging/appimage.sh"
fi

pnpm install --frozen-lockfile
cargo build --release --locked -p fernsicht-host-agent --features vaapi,kms,nvidia
cargo build --release --locked -p fernsicht-client --features vaapi,nvidia,window

# Tauri takes extra programs as "sidecars", named with the target triple.
triple=$(rustc -vV | sed -n 's/^host: //p')
mkdir -p apps/desktop/binaries
for bin in fernsicht-client fernsicht-host-agent; do
    cp "target/release/$bin" "apps/desktop/binaries/$bin-$triple"
done

cd apps/desktop
pnpm exec tauri build --bundles appimage

# The GPU stack stays the system's: a bundled libva looks for drivers where
# the build machine has them and may be older than the installed Mesa or
# NVIDIA driver, and hardware video would fail; the Vulkan loader must find
# the system's drivers too. The bundler takes them along, so they are taken
# out of its AppDir and the image is packed again.
appdir=target/release/bundle/appimage/Fernsicht.AppDir
for lib in libva.so libva-drm.so libva-x11.so libva-wayland.so libdrm.so libdrm_ \
    libvdpau.so libvulkan.so libgbm.so libEGL.so libGL.so libGLX.so libcuda.so libnvidia; do
    rm -f "$appdir/usr/lib/$lib"*
done

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
out=target/release/bundle/appimage/Fernsicht-$version-x86_64.AppImage
tool=../../target/tools/appimagetool
if [ ! -x "$tool" ]; then
    mkdir -p ../../target/tools
    curl -fsSL -o "$tool" \
        https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage
    chmod +x "$tool"
fi
ARCH=x86_64 "$tool" --appimage-extract-and-run --no-appstream "$appdir" "$out"
rm -f target/release/bundle/appimage/Fernsicht_*.AppImage
ls -l "$out"
