#!/usr/bin/env bash
# What can this machine's GPU do for Fernsicht? Reports the GPU, driver,
# VAAPI and Vulkan Video capabilities, and smoke-tests hardware H.264 encode
# and decode with FFmpeg (1080p60, 2 s), the same paths our backends use.
#
#   dev/gpu-check.sh                    report only, always exits 0
#   dev/gpu-check.sh --expect amd       fail unless VAAPI encode+decode work
#   dev/gpu-check.sh --expect nvidia    fail unless NVENC encode+NVDEC decode work
#   dev/gpu-check.sh --summary FILE     also append a Markdown report to FILE
#                                       (CI passes $GITHUB_STEP_SUMMARY)
set -uo pipefail

expect=""
summary=""
while [ $# -gt 0 ]; do
  case "$1" in
    --expect) expect="${2:-}"; shift 2 ;;
    --summary) summary="${2:-}"; shift 2 ;;
    -h | --help) sed -n '2,11p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$expect" in "" | amd | nvidia) ;; *) echo "--expect amd|nvidia" >&2; exit 2 ;; esac

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
results=()   # "name|PASS/FAIL/SKIP|detail"

section() { printf '\n== %s ==\n' "$1"; }
have() { command -v "$1" >/dev/null 2>&1; }
record() { results+=("$1|$2|$3"); printf '%-22s %-4s %s\n' "$1" "$2" "$3"; }

# --- Inventory --------------------------------------------------------------
section "GPU"
if have lspci; then
  lspci -nn | grep -Ei 'vga|3d|display' || echo "(keine GPU gefunden)"
else
  echo "(lspci fehlt)"
fi

section "DRM-Geräte"
ls -l /dev/dri 2>/dev/null || echo "(kein /dev/dri: Container ohne GPU-Zugriff?)"
render=""
for node in /dev/dri/renderD*; do
  [ -e "$node" ] && { render="$node"; break; }
done

nvidia_driver=""
if have nvidia-smi && nvidia-smi >/dev/null 2>&1; then
  section "NVIDIA"
  nvidia-smi --query-gpu=name,driver_version,vbios_version --format=csv,noheader
  nvidia_driver="$(nvidia-smi --query-gpu=driver_version --format=csv,noheader | head -n1)"
fi

section "VAAPI"
va_encode="no"
va_decode="no"
if [ -n "$render" ] && have vainfo; then
  vainfo --display drm --device "$render" >"$tmp/vainfo.txt" 2>&1
  grep -E 'Driver version|VAProfile(H264|HEVC|AV1)' "$tmp/vainfo.txt" | sed 's/^[[:space:]]*//'
  grep -Eq 'VAProfileH264(Main|High|ConstrainedBaseline).*VAEntrypointEncSlice' "$tmp/vainfo.txt" && va_encode="yes"
  grep -Eq 'VAProfileH264(Main|High|ConstrainedBaseline).*VAEntrypointVLD' "$tmp/vainfo.txt" && va_decode="yes"
else
  echo "(kein Render-Node oder vainfo fehlt)"
fi
echo "H.264 VAAPI encode: $va_encode, decode: $va_decode"

section "Vulkan Video"
vk_video=""
if have vulkaninfo; then
  vulkaninfo --summary 2>/dev/null | grep -E 'deviceName|driverName|driverInfo' | sed 's/^[[:space:]]*//'
  vk_video="$(vulkaninfo 2>/dev/null | grep -oE 'VK_KHR_video_(decode|encode)_(h264|h265|av1)' | sort -u | tr '\n' ' ')"
  echo "Extensions: ${vk_video:-(keine)}"
else
  echo "(vulkaninfo fehlt)"
fi

# --- Hardware codec smoke tests ---------------------------------------------
# Encode 2 s of 1080p60 test pattern, then decode the result in hardware.
# Reported fps is wall-clock throughput including FFmpeg overhead.
section "FFmpeg-Smoke-Tests (1080p60, 120 Frames)"
src=(-f lavfi -i "testsrc2=size=1920x1080:rate=60" -frames:v 120)

run_timed() { # name, output file, ffmpeg args...
  local name="$1" out="$2"
  shift 2
  local start end ms
  start="$(date +%s%N)"
  if ffmpeg -hide_banner -loglevel error -y "$@" >"$tmp/$name.log" 2>&1 && { [ "$out" = "-" ] || [ -s "$out" ]; }; then
    end="$(date +%s%N)"
    ms=$(((end - start) / 1000000))
    record "$name" PASS "$((120 * 1000 / (ms > 0 ? ms : 1))) fps"
    return 0
  fi
  record "$name" FAIL "$(tail -n1 "$tmp/$name.log" | cut -c1-120)"
  return 1
}

if ! have ffmpeg; then
  record "ffmpeg" FAIL "nicht installiert"
else
  if [ -n "$render" ]; then
    if run_timed vaapi-h264-encode "$tmp/vaapi.mp4" \
      -init_hw_device "vaapi=va:$render" -filter_hw_device va "${src[@]}" \
      -vf 'format=nv12,hwupload' -c:v h264_vaapi -bf 0 "$tmp/vaapi.mp4"; then
      run_timed vaapi-h264-decode - -hwaccel vaapi -hwaccel_device "$render" \
        -i "$tmp/vaapi.mp4" -f null -
    else
      record vaapi-h264-decode SKIP "kein Encode-Ergebnis"
    fi
  else
    record vaapi-h264-encode SKIP "kein Render-Node"
    record vaapi-h264-decode SKIP "kein Render-Node"
  fi

  if [ -n "$nvidia_driver" ]; then
    if run_timed nvenc-h264-encode "$tmp/nvenc.mp4" "${src[@]}" \
      -c:v h264_nvenc -preset p1 -tune ull -bf 0 "$tmp/nvenc.mp4"; then
      run_timed nvdec-h264-decode - -hwaccel cuda -i "$tmp/nvenc.mp4" -f null -
    else
      record nvdec-h264-decode SKIP "kein Encode-Ergebnis"
    fi
  else
    record nvenc-h264-encode SKIP "kein NVIDIA-Treiber"
    record nvdec-h264-decode SKIP "kein NVIDIA-Treiber"
  fi
fi

# --- Verdict ----------------------------------------------------------------
status_of() {
  local r
  for r in "${results[@]}"; do
    [ "${r%%|*}" = "$1" ] && { r="${r#*|}"; echo "${r%%|*}"; return; }
  done
  echo SKIP
}

required=()
case "$expect" in
  amd) required=(vaapi-h264-encode vaapi-h264-decode) ;;
  nvidia) required=(nvenc-h264-encode nvdec-h264-decode) ;;
esac
failed=()
for name in "${required[@]}"; do
  [ "$(status_of "$name")" = PASS ] || failed+=("$name")
done

if [ -n "$summary" ]; then
  {
    echo "### GPU-Check: $(hostname)${expect:+ (erwartet: $expect)}"
    echo
    echo "| Test | Ergebnis | Detail |"
    echo "|---|---|---|"
    for r in "${results[@]}"; do
      IFS='|' read -r n s d <<<"$r"
      echo "| $n | $s | ${d//|//} |"
    done
    echo
    echo "- VAAPI H.264: Encode $va_encode, Decode $va_decode"
    echo "- Vulkan Video: ${vk_video:-keine}"
    [ -n "$nvidia_driver" ] && echo "- NVIDIA-Treiber: $nvidia_driver"
  } >>"$summary"
fi

if [ ${#failed[@]} -gt 0 ]; then
  printf '\nFEHLGESCHLAGEN (erwartet für %s): %s\n' "$expect" "${failed[*]}"
  exit 1
fi
printf '\nOK\n'
