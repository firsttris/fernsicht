#!/bin/sh
# Builds the SDL3 the AppImage carries for gamepads (crates/input/src/sdl.rs
# loads it at runtime): a shared library with only the joystick, gamepad,
# HIDAPI, haptic and sensor parts, no video, audio or rendering. zlib-style
# licence. The release builds on an old distribution (wide glibc
# compatibility) that has no SDL3 of its own; where a distribution has one,
# the AppImage's is the one the client takes.
#
#   ./packaging/sdl3.sh [prefix]    (default: target/sdl3)
#
# Needs a C compiler, cmake, ninja or make, curl and the libudev headers
# (without them SDL finds pads only by watching /dev/input, and hot-plugging
# is less reliable). A prefix already built with this version is kept.
set -eu

sdl_version=3.4.18
sdl_sha256=9c75cf16330322c217dedd2e0609f1124f1b54b8633e763467b4684d0f4334a3

cd "$(dirname "$0")/.."
prefix=$(realpath -m "${1:-target/sdl3}")
stamp="$prefix/.built-$sdl_version"
if [ -e "$stamp" ]; then
    echo "SDL $sdl_version is in $prefix"
    exit 0
fi
work=$(realpath -m target/sdl3-build)
rm -rf "$work" "$prefix"
mkdir -p "$work"

tarball="$work/SDL3-$sdl_version.tar.gz"
curl -fsSL -o "$tarball" \
    "https://github.com/libsdl-org/SDL/releases/download/release-$sdl_version/SDL3-$sdl_version.tar.gz"
echo "$sdl_sha256  $tarball" | sha256sum --check --quiet
tar -xzf "$tarball" -C "$work"

# SDL_DEPS_SHARED (the default): libudev is loaded at runtime, so the
# library needs nothing beyond glibc.
cmake -S "$work/SDL3-$sdl_version" -B "$work/build" \
    -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$prefix" -DCMAKE_INSTALL_LIBDIR=lib \
    -DSDL_SHARED=ON -DSDL_STATIC=OFF -DSDL_TEST_LIBRARY=OFF -DSDL_TESTS=OFF \
    -DSDL_EXAMPLES=OFF -DSDL_INSTALL_DOCS=OFF \
    -DSDL_AUDIO=OFF -DSDL_VIDEO=OFF -DSDL_GPU=OFF -DSDL_RENDER=OFF -DSDL_CAMERA=OFF \
    -DSDL_POWER=OFF -DSDL_DIALOG=OFF -DSDL_DBUS=OFF -DSDL_IBUS=OFF -DSDL_LIBURING=OFF \
    -DSDL_UNIX_CONSOLE_BUILD=ON \
    -DSDL_JOYSTICK=ON -DSDL_HAPTIC=ON -DSDL_HIDAPI=ON -DSDL_SENSOR=ON -DSDL_LIBUDEV=ON

# libudev must have made it in (a missing header disables it quietly).
if ! grep -rqs '^#define SDL_UDEV_DYNAMIC' "$work/build"/include-config-*/; then
    echo "SDL was configured without libudev (install its headers)" >&2
    exit 1
fi

cmake --build "$work/build" -j"$(nproc)"
cmake --install "$work/build"
rm -rf "$work"
touch "$stamp"
echo "SDL $sdl_version is in $prefix"
