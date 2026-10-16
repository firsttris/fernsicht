#!/bin/sh
# After the .deb or .rpm is installed or updated (packaging/nfpm.yaml): the
# service is enabled and (re)started with the new host, as install.sh and the
# app do.
set -e

# A host installed before by the app (“Diesen Rechner freigeben”) or by
# install.sh: its unit in /etc would shadow the package's, its program the
# package's command.
rm -f /etc/systemd/system/fernsicht-host-agent.service /etc/systemd/system/fernsicht-host.service
case "$(readlink /usr/local/bin/fernsicht-host-agent 2>/dev/null || true)" in
/opt/fernsicht/*) rm -f /usr/local/bin/fernsicht-host-agent ;;
esac
rm -rf /opt/fernsicht/host /opt/fernsicht/app
rmdir /opt/fernsicht 2>/dev/null || true

# Let clients reach the port if a firewall is up: UDP for the app, TCP for
# the web viewer.
if command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    changed=0
    for proto in udp tcp; do
        if ! firewall-cmd --query-port="47800/$proto" >/dev/null 2>&1; then
            firewall-cmd --permanent --add-port="47800/$proto" >/dev/null
            changed=1
        fi
    done
    if [ "$changed" = 1 ]; then firewall-cmd --reload >/dev/null; fi
fi

# Not in a container or chroot without systemd: there it starts at boot.
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable fernsicht-host-agent.service
    systemctl restart fernsicht-host-agent.service ||
        echo "fernsicht-host-agent startet nicht: journalctl -u fernsicht-host-agent" >&2
else
    systemctl enable fernsicht-host-agent.service 2>/dev/null || true
fi
echo "Fernsicht-Host installiert. Gerät koppeln: sudo fernsicht-host-agent pair"
