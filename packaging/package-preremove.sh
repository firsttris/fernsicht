#!/bin/sh
# Before the .deb or .rpm is removed (packaging/nfpm.yaml): the service stops.
# Not on an update (deb: "upgrade", rpm: 1), whose postinstall restarts it.
set -e
case "$1" in
remove | purge | 0)
    systemctl disable --now fernsicht-host-agent.service 2>/dev/null || true
    ;;
esac
