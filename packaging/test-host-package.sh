#!/bin/sh
# Installs the host's archive (through install.sh, as `curl … | sudo sh`
# does) and its .deb or .rpm on a fresh distribution, checks that the host
# starts and serves the web viewer, and removes both again. Run as root in a
# container of that distribution (release.yml: Arch, Fedora, Debian,
# Ubuntu), with packaging/host-bundle.sh's files in DIST:
#
#   docker run --rm -v "$PWD:/src" -w /src archlinux packaging/test-host-package.sh target/host-bundle
#
# A container has no systemd: the service is set up, not started; the host
# is started by hand instead.
set -eu

dist=$(cd "${1:?usage: $0 DIST}" && pwd)
# shellcheck source=/dev/null
. /etc/os-release

step() { printf '\n── %s\n' "$*"; }
fail() {
    echo "test-host-package: $*" >&2
    exit 1
}

archive=$(cd "$dist" && ls fernsicht-host-*-x86_64.tar.gz)
version=${archive#fernsicht-host-}
version=${version%-x86_64.tar.gz}

# The host at $1 starts on its own ports and serves the viewer it finds
# beside itself.
starts() {
    tmp=$(mktemp -d)
    "$1" --capture test-pattern --encoder synthetic --bind 127.0.0.1:47910 \
        --web 127.0.0.1:47911 --state-dir "$tmp" --control "$tmp/control.sock" \
        >"$tmp/log" 2>&1 &
    pid=$!
    page=
    for _ in $(seq 50); do
        page=$(curl -fsS http://127.0.0.1:47911/ 2>/dev/null || true)
        [ -n "$page" ] && break
        sleep 0.2
    done
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    case "$page" in
    *"<html"*) echo "$1 serves the web viewer" ;;
    *) cat "$tmp/log" >&2 && fail "$1 serves no web viewer" ;;
    esac
    rm -rf "$tmp"
}

is_version() {
    out=$("$1" --version)
    [ "$out" = "fernsicht-host-agent $version" ] || fail "$1 --version: $out, expected $version"
}

gone() {
    for path in "$@"; do
        if [ -e "$path" ] || [ -L "$path" ]; then fail "$path is still there"; fi
    done
}

# systemctl, as on any host the service runs on (not systemd itself: it
# does not run in a container).
step "System packages ($ID)"
case "$ID" in
arch)
    # No package for Arch: libva and libdrm as install.sh asks for them.
    pacman -Syu --noconfirm --needed libva libdrm curl systemd >/dev/null
    ;;
fedora)
    dnf install -y -q curl findutils systemd
    ;;
debian | ubuntu)
    apt-get update -qq
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        curl ca-certificates systemd >/dev/null
    ;;
*) fail "unknown distribution $ID" ;;
esac

# First the package: its dependencies come from the package manager, so
# they are complete only if the package names them.
case "$ID" in
fedora | debian | ubuntu)
    step "Package"
    if [ "$ID" = fedora ]; then
        dnf install -y -q "$dist"/fernsicht-host-"$version"-1.x86_64.rpm
    else
        DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "$dist"/fernsicht-host_"$version"-1_amd64.deb >/dev/null
    fi
    is_version /usr/bin/fernsicht-host-agent
    starts /usr/bin/fernsicht-host-agent
    [ -f /usr/lib/systemd/system/fernsicht-host-agent.service ] || fail "the package has no unit"
    grep -q '^ExecStart=/usr/lib/fernsicht-host/bin/fernsicht-host-agent ' \
        /usr/lib/systemd/system/fernsicht-host-agent.service || fail "the package's unit starts something else"
    # install.sh leaves a package to its package manager.
    if sh "$dist/install.sh" 2>/dev/null; then fail "install.sh replaced the package"; fi
    if [ "$ID" = fedora ]; then dnf remove -y -q fernsicht-host; else apt-get remove -y -qq fernsicht-host >/dev/null; fi
    gone /usr/lib/fernsicht-host /usr/bin/fernsicht-host-agent /usr/lib/systemd/system/fernsicht-host-agent.service
    ;;
esac

# What install.sh asks for when it is missing (dnf removes the package's
# dependencies along with it).
case "$ID" in
fedora) dnf install -y -q libva libdrm ;;
debian | ubuntu)
    DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
        libva2 libva-drm2 libdrm2 >/dev/null
    ;;
esac

step "Archive through install.sh, piped into sh"
FERNSICHT_DOWNLOAD_URL="file://$dist" sh <"$dist/install.sh"
is_version /usr/local/bin/fernsicht-host-agent
is_version /opt/fernsicht/host/bin/fernsicht-host-agent
starts /usr/local/bin/fernsicht-host-agent
grep -q '^ExecStart=/opt/fernsicht/host/bin/fernsicht-host-agent ' \
    /etc/systemd/system/fernsicht-host-agent.service || fail "the unit starts something else"

step "Update from the installed copy"
FERNSICHT_DOWNLOAD_URL="file://$dist" /opt/fernsicht/host/install.sh
is_version /usr/local/bin/fernsicht-host-agent

step "A damaged download is refused"
bad=$(mktemp -d)
cp "$dist/SHA256SUMS" "$bad/"
printf 'not the archive' >"$bad/$archive"
if FERNSICHT_DOWNLOAD_URL="file://$bad" sh <"$dist/install.sh" 2>"$bad/err"; then
    fail "a download that does not match SHA256SUMS was installed"
fi
grep -q SHA256SUMS "$bad/err" || fail "refused for another reason: $(cat "$bad/err")"
is_version /usr/local/bin/fernsicht-host-agent

step "Uninstall"
/opt/fernsicht/host/install.sh --uninstall
gone /opt/fernsicht /usr/local/bin/fernsicht-host-agent /etc/systemd/system/fernsicht-host-agent.service

step "All green on $PRETTY_NAME"
