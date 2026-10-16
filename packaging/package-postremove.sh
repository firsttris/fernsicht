#!/bin/sh
# After the .deb or .rpm is removed (packaging/nfpm.yaml). Key and paired
# devices stay in /var/lib/fernsicht.
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi
