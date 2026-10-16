#!/bin/sh
# Installs (or updates) the Fernsicht host as a system service, without the
# app: for a server reached over SSH, or any Linux with systemd.
#
#   curl -fsSL https://github.com/firsttris/fernsicht/releases/latest/download/install.sh | sudo sh
#   sudo /opt/fernsicht/host/install.sh              update to the latest release
#   sudo /opt/fernsicht/host/install.sh --uninstall  remove it
#
# Run from an unpacked fernsicht-host-<version>-x86_64.tar.gz, it installs
# that archive's host. Otherwise it downloads the release's archive and
# checks it against the release's SHA256SUMS. FERNSICHT_VERSION=0.1.7 picks
# a release instead of the latest, FERNSICHT_DOWNLOAD_URL a directory that
# holds the files (a mirror; the tests use a file:// one). Key and paired
# devices stay in /var/lib/fernsicht either way.
#
# Everything runs from the last line, so a download cut off midway runs
# nothing. Built into the archive by packaging/host-bundle.sh.
set -eu

repo=https://github.com/firsttris/fernsicht
dir=/opt/fernsicht/host
unit=fernsicht-host-agent.service
unit_path=/etc/systemd/system/$unit
# The .deb's and .rpm's unit (packaging/nfpm.yaml): updated by the package
# manager, not by this script.
packaged_unit=/usr/lib/systemd/system/$unit
# The service's old name (before 0.2), removed when installing or removing.
old_unit=fernsicht-host.service
link=/usr/local/bin/fernsicht-host-agent
port=47800

fail() {
    echo "fernsicht: $*" >&2
    exit 1
}

# Where the release's files are.
release_url() {
    if [ -n "${FERNSICHT_DOWNLOAD_URL:-}" ]; then
        echo "${FERNSICHT_DOWNLOAD_URL%/}"
    elif [ -n "${FERNSICHT_VERSION:-}" ]; then
        echo "$repo/releases/download/v${FERNSICHT_VERSION#v}"
    else
        echo "$repo/releases/latest/download"
    fi
}

# fetch URL FILE
fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https,file' -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        fail "curl oder wget fehlt"
    fi
}

# The archive named in SHA256SUMS, downloaded into $1, checked and unpacked;
# prints the unpacked directory.
download() {
    url=$(release_url)
    fetch "$url/SHA256SUMS" "$1/SHA256SUMS" || fail "$url/SHA256SUMS nicht erreichbar"
    name=$(sed -n 's/^[0-9a-f]\{64\}  \(fernsicht-host-[0-9][0-9A-Za-z.+-]*-x86_64\.tar\.gz\)$/\1/p' \
        "$1/SHA256SUMS" | head -n 1)
    [ -n "$name" ] || fail "kein Host-Archiv in $url/SHA256SUMS"
    echo "Lade $name …" >&2
    fetch "$url/$name" "$1/$name" || fail "$url/$name nicht erreichbar"
    (cd "$1" && grep "  $name\$" SHA256SUMS | sha256sum -c --quiet -) >&2 ||
        fail "$name stimmt nicht mit SHA256SUMS überein"
    tar -xzf "$1/$name" -C "$1"
    echo "$1/${name%.tar.gz}"
}

# Libraries the host needs from the system (the GPU's: libva, libdrm; FFmpeg
# comes with it). Prints what is missing.
missing_libraries() {
    if command -v ldd >/dev/null 2>&1; then
        ldd "$1" | sed -n 's/^[[:space:]]*\([^ ]*\) => not found.*/\1/p' | sort -u | xargs
    fi
}

firewall() {
    # UDP for the app, TCP for the web viewer (its WebRTC uses a random UDP
    # port above 1024).
    if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
        changed=0
        for proto in udp tcp; do
            if ! firewall-cmd --query-port="$port/$proto" >/dev/null 2>&1; then
                firewall-cmd --permanent --add-port="$port/$proto" >/dev/null
                changed=1
                echo "Firewall: $proto-Port $port geöffnet."
            fi
        done
        if [ "$changed" = 1 ]; then firewall-cmd --reload >/dev/null; fi
    fi
}

# The address a browser in the LAN opens.
address() {
    ip -4 -o route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p' | grep . ||
        echo "<Adresse des Rechners>"
}

# install_from DIR: DIR is an unpacked archive.
install_from() {
    src=$1
    host="$src/bin/fernsicht-host-agent"
    if [ ! -x "$host" ] || [ ! -f "$src/$unit" ]; then
        fail "$src ist kein Fernsicht-Host-Archiv"
    fi
    missing=$(missing_libraries "$host")
    if [ -n "$missing" ]; then
        echo "fernsicht: auf diesem System fehlen Bibliotheken:" "$missing" >&2
        echo "  Arch:          pacman -S libva libdrm" >&2
        echo "  Debian/Ubuntu: apt install libva2 libva-drm2 libdrm2" >&2
        echo "  Fedora:        dnf install libva libdrm" >&2
        exit 1
    fi
    version=$("$host" --version) || fail "$host startet nicht"
    version="Fernsicht-Host ${version#* }"

    # Copied next to the old one first, then swapped: an update that fails
    # halfway leaves the running host as it was.
    mkdir -p "${dir%/*}"
    new=$(mktemp -d "${dir%/*}/.host-new.XXXXXX")
    cp -R "$src/bin" "$src/lib" "$src/share" "$src/install.sh" "$new/"
    chown -R root:root "$new"
    chmod -R u+rwX,go-w,go+rX "$new"
    chmod 755 "$new"

    systemctl stop "$unit" 2>/dev/null || true
    systemctl disable --now "$old_unit" 2>/dev/null || true
    rm -f "/etc/systemd/system/$old_unit"
    rm -rf "$dir"
    mv "$new" "$dir"
    # The app's copy of the host (“Diesen Rechner freigeben”): this unit
    # replaces the one that started it.
    rm -rf "${dir%/*}/app"
    if command -v restorecon >/dev/null 2>&1; then restorecon -R "${dir%/*}" || true; fi
    install -m 0644 "$src/$unit" "$unit_path"
    mkdir -p "${link%/*}"
    ln -sf "$dir/bin/fernsicht-host-agent" "$link"

    firewall
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload
        systemctl enable "$unit" >/dev/null 2>&1
        systemctl restart "$unit"
        # A host that finds no monitor or encoder ends right away.
        sleep 2
        if systemctl is-active --quiet "$unit"; then
            echo "$version läuft."
        else
            echo "$version installiert, startet aber nicht; der Grund steht im Log:" >&2
            journalctl -u "$unit" -n 5 --no-pager -o cat >&2 2>/dev/null || true
        fi
    else
        # A container or chroot: installed, started at the next boot.
        systemctl enable "$unit" >/dev/null 2>&1 || true
        echo "$version installiert (systemd läuft nicht: startet beim nächsten Boot)."
    fi
    echo "  Gerät koppeln: sudo fernsicht-host-agent pair"
    echo "  Im Browser:    http://$(address):$port"
    echo "  Log:           journalctl -u $unit -f"
    echo "  Entfernen:     sudo $dir/install.sh --uninstall"
}

uninstall() {
    systemctl disable --now "$unit" "$old_unit" 2>/dev/null || true
    rm -f "$unit_path" "/etc/systemd/system/$old_unit"
    if [ -L "$link" ]; then rm -f "$link"; fi
    rm -rf "$dir"
    rmdir "${dir%/*}" 2>/dev/null || true
    if [ -d /run/systemd/system ]; then systemctl daemon-reload; fi
    echo "Fernsicht-Host entfernt (Schlüssel und Kopplungen bleiben in /var/lib/fernsicht)."
}

main() {
    case "${1:-}" in
    "" | --uninstall) ;;
    -h | --help)
        sed -n '2,16s/^# \{0,1\}//p' "$0" 2>/dev/null ||
            echo "curl -fsSL $repo/releases/latest/download/install.sh | sudo sh"
        exit 0
        ;;
    *) fail "unbekannte Option $1 (--uninstall oder --help)" ;;
    esac
    [ "$(id -u)" -eq 0 ] || fail "bitte mit sudo starten"
    command -v systemctl >/dev/null 2>&1 || fail "der Host läuft als systemd-Dienst; systemctl fehlt"
    if [ -f "$packaged_unit" ]; then
        fail "als Paket installiert (.deb/.rpm): mit apt oder dnf aktualisieren und entfernen"
    fi
    if [ "${1:-}" = --uninstall ]; then
        uninstall
        return
    fi
    [ "$(uname -m)" = x86_64 ] || fail "nur für x86_64 gebaut, dieser Rechner ist $(uname -m)"

    # Run from an unpacked archive: that one. Piped into sh, or run from the
    # installed copy (an update): the release's.
    here=
    case "$0" in
    */install.sh) here=$(cd "$(dirname "$0")" && pwd) ;;
    esac
    if [ -n "$here" ] && [ "$here" != "$dir" ] && [ -x "$here/bin/fernsicht-host-agent" ]; then
        install_from "$here"
    else
        tmp=$(mktemp -d)
        trap 'rm -rf "$tmp"' EXIT
        bundle=$(download "$tmp")
        install_from "$bundle"
    fi
}

main "$@"
