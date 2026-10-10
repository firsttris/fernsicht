#!/bin/sh
# Installs (or updates) the Fernsicht host as a system service.
#
#   cargo build --release -p fernsicht-host-agent --features vaapi,kms,nvidia
#   sudo ./packaging/install-host.sh
#
# Remove it again with: sudo ./packaging/install-host.sh --uninstall
# Key and paired devices stay in /var/lib/fernsicht either way.
set -eu

cd "$(dirname "$0")/.."
bin=target/release/fernsicht-host-agent
unit=fernsicht-host.service
prefix=${PREFIX:-/usr/local}
port=47800

if [ "$(id -u)" -ne 0 ]; then
    echo "Bitte mit sudo starten: sudo $0 $*" >&2
    exit 1
fi

if [ "${1:-}" = "--uninstall" ]; then
    systemctl disable --now "$unit" 2>/dev/null || true
    rm -f "/etc/systemd/system/$unit" "$prefix/bin/fernsicht-host-agent"
    systemctl daemon-reload
    echo "Fernsicht-Host entfernt (Schlüssel und Kopplungen bleiben in /var/lib/fernsicht)."
    exit 0
fi

if [ ! -x "$bin" ]; then
    echo "$bin fehlt. Erst bauen (als normaler Benutzer):" >&2
    echo "  cargo build --release -p fernsicht-host-agent --features vaapi,kms,nvidia" >&2
    exit 1
fi
# Built in a distrobox, the libraries must exist on the host as well.
missing=$(ldd "$bin" | grep "not found" || true)
if [ -n "$missing" ]; then
    echo "Auf diesem System fehlen Bibliotheken für $bin:" >&2
    echo "$missing" >&2
    exit 1
fi

install -D -m 0755 "$bin" "$prefix/bin/fernsicht-host-agent"
sed "s|/usr/local/bin/|$prefix/bin/|" "packaging/$unit" >"/etc/systemd/system/$unit"
chmod 0644 "/etc/systemd/system/$unit"
systemctl daemon-reload
systemctl enable "$unit"
systemctl restart "$unit"

# Let clients reach the port if a firewall is up.
if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    if ! firewall-cmd --query-port="$port/udp" >/dev/null 2>&1; then
        firewall-cmd --permanent --add-port="$port/udp" >/dev/null
        firewall-cmd --reload >/dev/null
        echo "Firewall: UDP-Port $port geöffnet."
    fi
fi

sleep 1
if systemctl is-active --quiet "$unit"; then
    echo "Fernsicht-Host läuft. Gerät koppeln: fernsicht-host-agent pair"
else
    echo "Der Dienst startet nicht. Log: journalctl -u $unit -n 50" >&2
    exit 1
fi
