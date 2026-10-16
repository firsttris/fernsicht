#!/bin/sh
# Packs the host for servers and other computers without the app:
# fernsicht-host-<version>-x86_64.tar.gz (the host, the FFmpeg it was built
# against, the web viewer, the service and install.sh) and, from the same
# files, fernsicht-host_<version>-1_amd64.deb and
# fernsicht-host-<version>-1.x86_64.rpm (nfpm, packaging/nfpm.yaml); next to
# them install.sh and SHA256SUMS. The GPU's libraries (libva, libdrm,
# Vulkan, CUDA) stay the system's, as in the AppImage (packaging/appimage.sh).
#
#   ./packaging/ffmpeg.sh target/ffmpeg
#   PKG_CONFIG_PATH=$PWD/target/ffmpeg/lib/pkgconfig \
#     cargo build --release --locked -p fernsicht-host-agent --features vaapi,kms,nvidia,vulkan
#   pnpm --filter @fernsicht/viewer build
#   ./packaging/host-bundle.sh [out-dir]       (default: target/host-bundle)
#
# FFMPEG_LIB names another FFmpeg than target/ffmpeg/lib. Without nfpm (or
# NFPM=path) only the archive is made. Needs patchelf.
set -eu

cd "$(dirname "$0")/.."
out=$(realpath -m "${1:-target/host-bundle}")
ffmpeg_lib=$(realpath -m "${FFMPEG_LIB:-target/ffmpeg/lib}")
bin=target/release/fernsicht-host-agent
viewer=web/viewer/dist
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
name=fernsicht-host-$version-x86_64
stage=$out/$name

fail() {
    echo "host-bundle: $*" >&2
    exit 1
}
[ -x "$bin" ] || fail "$bin fehlt (cargo build --release -p fernsicht-host-agent …)"
[ -f "$viewer/index.html" ] || fail "$viewer fehlt (pnpm --filter @fernsicht/viewer build)"
command -v patchelf >/dev/null 2>&1 || fail "patchelf fehlt"

# FFmpeg's libraries an ELF file needs (by soname).
needed() {
    readelf -d "$1" | sed -n 's/.*(NEEDED).*\[\(libav[a-z]*\.so\.[0-9]*\)\]/\1/p'
}

rm -rf "$stage"
mkdir -p "$stage/bin" "$stage/lib" "$stage/share/fernsicht"
cp "$bin" "$stage/bin/"
# FFmpeg next to the host, and the libraries those need in turn.
todo=$(needed "$bin")
while [ -n "$todo" ]; do
    next=
    for lib in $todo; do
        [ -e "$stage/lib/$lib" ] && continue
        [ -e "$ffmpeg_lib/$lib" ] || fail "$lib ist nicht in $ffmpeg_lib"
        cp -L "$ffmpeg_lib/$lib" "$stage/lib/"
        next="$next $(needed "$stage/lib/$lib")"
    done
    todo=$(echo "$next" | xargs)
done
[ -n "$(ls "$stage/lib")" ] || fail "$bin braucht kein FFmpeg? (gebaut ohne vaapi/nvidia?)"
# Found beside the host wherever it is installed (/opt/fernsicht/host from
# the archive, /usr/lib/fernsicht-host from a package).
# shellcheck disable=SC2016 # $ORIGIN is for the dynamic loader
patchelf --set-rpath '$ORIGIN/../lib' "$stage/bin/fernsicht-host-agent"
for lib in "$stage"/lib/*; do
    # shellcheck disable=SC2016
    patchelf --set-rpath '$ORIGIN' "$lib"
done
# Where the host looks for its viewer: ../share/fernsicht/viewer beside it.
cp -R "$viewer" "$stage/share/fernsicht/viewer"
cp packaging/install.sh "$stage/"
cp LICENSE NOTICE "$stage/"
# The service, starting the host where install.sh puts it.
sed "s|^ExecStart=/usr/local/bin/|ExecStart=/opt/fernsicht/host/bin/|" \
    packaging/fernsicht-host-agent.service >"$stage/fernsicht-host-agent.service"
grep -q '^ExecStart=/opt/fernsicht/host/bin/fernsicht-host-agent ' "$stage/fernsicht-host-agent.service" ||
    fail "ExecStart in packaging/fernsicht-host-agent.service nicht gefunden"
chmod -R u+rwX,go-w,go+rX "$stage"

# The same bytes from the same commit: owners, order and times fixed.
epoch=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || echo 0)}
tar --owner=0 --group=0 --numeric-owner --sort=name --mtime="@$epoch" \
    -C "$out" -czf "$out/$name.tar.gz" "$name"
# The installer beside it, as the release offers it (`curl … | sudo sh`).
cp packaging/install.sh "$out/install.sh"
ls -l "$out/$name.tar.gz"

# What install.sh checks the download against.
sums() {
    (cd "$out" && find . -maxdepth 1 -type f ! -name 'SHA256SUMS*' -printf '%f\n' | sort |
        xargs sha256sum -- >SHA256SUMS.part && mv SHA256SUMS.part SHA256SUMS)
}
nfpm=${NFPM:-$(command -v nfpm || true)}
if [ -z "$nfpm" ]; then
    echo "nfpm fehlt: kein .deb und .rpm" >&2
    rm -f "$out"/*.deb "$out"/*.rpm
    sums
    exit 0
fi
# The package's service starts the host where the package puts it.
pkg=$out/package
rm -rf "$pkg"
mkdir -p "$pkg"
sed "s|^ExecStart=/opt/fernsicht/host/bin/|ExecStart=/usr/lib/fernsicht-host/bin/|" \
    "$stage/fernsicht-host-agent.service" >"$pkg/fernsicht-host-agent.service"
# nfpm.yaml names the files by ${STAGE}, ${PACKAGE} and ${VERSION}.
sed -e "s|\${STAGE}|$stage|g" -e "s|\${PACKAGE}|$pkg|g" -e "s|\${VERSION}|$version|g" \
    packaging/nfpm.yaml >"$pkg/nfpm.yaml"
for packager in deb rpm; do
    SOURCE_DATE_EPOCH=$epoch "$nfpm" package --config "$pkg/nfpm.yaml" \
        --packager "$packager" --target "$out/"
done
sums
ls -l "$out"/*.deb "$out"/*.rpm
