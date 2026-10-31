#!/bin/sh
# Builds the FFmpeg the AppImage carries: shared libraries with only what
# Fernsicht uses (FFmpeg's H.264, HEVC and AV1 decoders with VAAPI, NVDEC and
# Vulkan Video; the VAAPI, NVENC and Vulkan Video encoders; scale_vaapi).
# LGPL only: no GPL or non-free parts, no other libraries bundled with it.
# The release builds on an old distribution (wide glibc compatibility) whose
# own FFmpeg is too old for Vulkan Video (7.1 or newer).
#
#   ./packaging/ffmpeg.sh [prefix]    (default: target/ffmpeg)
#
# Then build against it:
#   PKG_CONFIG_PATH=$prefix/lib/pkgconfig LD_LIBRARY_PATH=$prefix/lib
#
# Needs a C compiler, make, pkg-config, git, curl, xz and the libva and libdrm
# headers. A prefix already built with these versions is kept.
set -eu

ffmpeg_version=9.0.2
ffmpeg_sha256=7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3
# NVENC/NVDEC API (header only; CUDA is loaded at runtime). 12.1 is the
# oldest FFmpeg takes: NVENC then works with NVIDIA drivers from 530 on.
nvcodec_tag=n12.1.14.0
nvcodec_commit=1889e62e2d35ff7aa9baca2bceb14f053785e6f1
# Vulkan headers (the loader is the system's, loaded at runtime); 1.4.317 or
# newer enables the AV1 Vulkan encoder.
vulkan_tag=vulkan-sdk-1.4.341.0
vulkan_commit=b5c8f996196ba4aa6d8f97e52b5d3b6e70f7e4e2

cd "$(dirname "$0")/.."
prefix=$(realpath -m "${1:-target/ffmpeg}")
stamp="$prefix/.built-$ffmpeg_version-$nvcodec_tag-$vulkan_tag"
if [ -e "$stamp" ]; then
    echo "FFmpeg $ffmpeg_version is in $prefix"
    exit 0
fi
work=$(realpath -m target/ffmpeg-build)
rm -rf "$work" "$prefix"
mkdir -p "$work"

# A tag, checked against the commit it must be.
fetch() {
    git clone -q --depth 1 --branch "$2" "https://github.com/$1.git" "$4"
    if [ "$(git -C "$4" rev-parse HEAD)" != "$3" ]; then
        echo "$1 $2 is not commit $3" >&2
        exit 1
    fi
}
fetch FFmpeg/nv-codec-headers "$nvcodec_tag" "$nvcodec_commit" "$work/nv-codec-headers"
make -s -C "$work/nv-codec-headers" PREFIX="$work/headers" install
fetch KhronosGroup/Vulkan-Headers "$vulkan_tag" "$vulkan_commit" "$work/vulkan-headers"
cp -r "$work/vulkan-headers/include/vulkan" "$work/vulkan-headers/include/vk_video" \
    "$work/headers/include/"

tarball="$work/ffmpeg-$ffmpeg_version.tar.xz"
curl -fsSL -o "$tarball" "https://ffmpeg.org/releases/ffmpeg-$ffmpeg_version.tar.xz"
echo "$ffmpeg_sha256  $tarball" | sha256sum --check --quiet
tar -xf "$tarball" -C "$work"

cd "$work/ffmpeg-$ffmpeg_version"
# Nothing found by itself (--disable-autodetect): what is used is named.
# The decoders only parse, the GPU decodes; hand-written assembly would
# only speed up decoding on the CPU.
PKG_CONFIG_PATH="$work/headers/lib/pkgconfig" ./configure \
    --prefix="$prefix" \
    --enable-shared --disable-static \
    --disable-programs --disable-doc --disable-network --disable-x86asm \
    --disable-autodetect --disable-everything \
    --disable-avdevice --disable-swscale --disable-swresample \
    --enable-pthreads \
    --enable-libdrm --enable-vaapi \
    --enable-vulkan \
    --enable-ffnvcodec --enable-nvenc --enable-nvdec \
    --enable-decoder=h264,hevc,av1 \
    --enable-hwaccel=h264_vaapi,hevc_vaapi,av1_vaapi \
    --enable-hwaccel=h264_nvdec,hevc_nvdec,av1_nvdec \
    --enable-hwaccel=h264_vulkan,hevc_vulkan,av1_vulkan \
    --enable-encoder=h264_vaapi,hevc_vaapi,av1_vaapi \
    --enable-encoder=h264_nvenc,hevc_nvenc,av1_nvenc \
    --enable-encoder=h264_vulkan,hevc_vulkan,av1_vulkan \
    --enable-filter=buffer,buffersink,scale_vaapi \
    --extra-cflags="-I$work/headers/include"

# Everything named must have made it in (a missing header disables a
# component quietly).
for want in h264_decoder hevc_decoder av1_decoder \
    h264_vaapi_hwaccel hevc_vaapi_hwaccel av1_vaapi_hwaccel \
    h264_nvdec_hwaccel hevc_nvdec_hwaccel av1_nvdec_hwaccel \
    h264_vulkan_hwaccel hevc_vulkan_hwaccel av1_vulkan_hwaccel \
    h264_vaapi_encoder hevc_vaapi_encoder av1_vaapi_encoder \
    h264_nvenc_encoder hevc_nvenc_encoder av1_nvenc_encoder \
    h264_vulkan_encoder hevc_vulkan_encoder av1_vulkan_encoder \
    scale_vaapi_filter; do
    upper=$(echo "$want" | tr '[:lower:]' '[:upper:]')
    if ! grep -q "^CONFIG_$upper=yes" ffbuild/config.mak; then
        echo "FFmpeg was configured without $want" >&2
        exit 1
    fi
done

make -s -j"$(nproc)"
make -s install
cd - >/dev/null
rm -rf "$work"
touch "$stamp"
echo "FFmpeg $ffmpeg_version is in $prefix"
