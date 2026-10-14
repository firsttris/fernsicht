#!/usr/bin/env bash
# Connections over the internet through real NATs (docs/internet.md,
# “Testing”): network namespaces for the internet, two routers that NAT
# with nftables, a host behind one and a client behind the other. The
# internet runs coturn (STUN and TURN) and the rendezvous service. Each
# case pairs once (through a port forwarded for that moment), takes the
# forward away, and connects through the service; the client must say
# which path it took.
#
#   cone/cone            → hole punched
#   symmetric client     → through the relay
#   symmetric/symmetric  → through the relay
#
# Needs root (network namespaces), ip (iproute2), nft, conntrack, turnserver (coturn)
# and the built programs (cargo build -p fernsicht-host-agent
# -p fernsicht-client -p fernsicht-rendezvous); TARGET_DIR says where they
# are (default: target/debug).
set -euo pipefail
cd "$(dirname "$0")/../.."
bin=${TARGET_DIR:-target/debug}
for tool in ip nft turnserver; do
    command -v "$tool" >/dev/null || { echo "missing: $tool"; exit 1; }
done
[ "$(id -u)" = 0 ] || { echo "needs root (network namespaces)"; exit 1; }

work=$(mktemp -d)
pids=()
cleanup() {
    for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
    for ns in wan hr h cr c; do ip netns del "fs-$ns" 2>/dev/null || true; done
    if [ -n "${KEEP:-}" ]; then echo "logs in $work"; else rm -rf "$work"; fi
}
trap cleanup EXIT

inside() { local ns=$1; shift; ip netns exec "fs-$ns" "$@"; }

# A veth pair between two namespaces, with addresses.
link() { # ns1 if1 addr1 ns2 if2 addr2
    ip link add "$2" netns "fs-$1" type veth peer name "$5" netns "fs-$4"
    inside "$1" ip addr add "$3" dev "$2"
    inside "$4" ip addr add "$6" dev "$5"
    inside "$1" ip link set "$2" up
    inside "$4" ip link set "$5" up
}

# The internet (198.51.100.0/24) and two homes behind routers.
setup() { # host NAT, client NAT: cone or symmetric
    for ns in wan hr h cr c; do
        ip netns add "fs-$ns"
        inside "$ns" ip link set lo up
    done
    inside wan ip link add br0 type bridge
    inside wan ip addr add 198.51.100.1/24 dev br0
    inside wan ip link set br0 up
    for side in h c; do
        local n=10; [ "$side" = c ] && n=20
        ip link add "w$side" netns fs-wan type veth peer name wan netns "fs-${side}r"
        inside wan ip link set "w$side" master br0 up
        inside "${side}r" ip addr add "198.51.100.$n/24" dev wan
        inside "${side}r" ip link set wan up
        link "${side}r" lan "192.168.$n.1/24" "$side" eth0 "192.168.$n.2/24"
        inside "$side" ip route add default via "192.168.$n.1"
        inside "${side}r" sysctl -qw net.ipv4.ip_forward=1
        local how=$1; [ "$side" = c ] && how=$2
        local masq="masquerade"
        # A port per connection: what a symmetric NAT does.
        [ "$how" = symmetric ] && masq="masquerade random"
        # A firewall as home routers have: nothing new comes in from the
        # internet. (Without it, a probe arriving before the router's own
        # mapping exists leaves a conntrack entry that makes the NAT pick
        # another port for the way out.)
        inside "${side}r" nft -f - <<EOF
table ip filter {
    chain input { type filter hook input priority 0; iif "wan" ct state new drop; }
    chain forward { type filter hook forward priority 0; ct status dnat accept; iif "wan" ct state new drop; }
}
table ip nat {
    chain prerouting { type nat hook prerouting priority -100; }
    chain postrouting { type nat hook postrouting priority 100; oif "wan" $masq; }
}
EOF
    done
}

start_internet() {
    inside wan turnserver --listening-ip=198.51.100.1 --relay-ip=198.51.100.1 \
        --listening-port=3478 --use-auth-secret --static-auth-secret=north \
        --realm=fernsicht --no-tls --no-dtls --no-cli --no-tcp-relay -n --verbose \
        --log-file=stdout >"$work/turn.log" 2>&1 &
    pids+=($!)
    FERNSICHT_TURN_SECRET=north inside wan "$bin/fernsicht-rendezvous" \
        --listen 198.51.100.1:8080 --turn-url "turn:198.51.100.1:3478?transport=udp" \
        >"$work/rendezvous.log" 2>&1 &
    pids+=($!)
}

run_case() { # name host-NAT client-NAT expected-path
    local name=$1
    echo "── $name"
    setup "$2" "$3"
    start_internet
    local hs="$work/$name-host" cs="$work/$name-client"
    inside h "$bin/fernsicht-host-agent" --state-dir "$hs" --control "$hs/control.sock" \
        --no-web --pair --rendezvous http://198.51.100.1:8080 --stun 198.51.100.1:3478 \
        >"$work/$name-host.log" 2>&1 &
    pids+=($!)
    local pin=""
    for _ in $(seq 50); do
        pin=$(sed -n 's/.*PIN: \([0-9]*\).*/\1/p' "$work/$name-host.log" | head -1)
        [ -n "$pin" ] && grep -q "registered at the rendezvous" "$work/$name-host.log" && break
        sleep 0.2
    done
    [ -n "$pin" ] || { cat "$work/$name-host.log"; echo "no PIN"; exit 1; }
    # Pairing through a port forwarded for this moment.
    inside hr nft add rule ip nat prerouting iif wan udp dport 47800 dnat to 192.168.10.2
    inside c "$bin/fernsicht-client" pair 198.51.100.10 "$pin" --state-dir "$cs" --name laptop
    inside hr nft flush chain ip nat prerouting
    # Only the forward's own entry goes (the host's connection to the
    # service stays, as on a router that removes a forward).
    inside hr conntrack -D -p udp --orig-port-dst 47800 >/dev/null 2>&1 || true
    # Now only through the service.
    if ! RUST_LOG=${RUST_LOG:-info} inside c "$bin/fernsicht-client" 198.51.100.10 --headless --no-audio --duration 3 \
        --width 640 --height 360 --state-dir "$cs" \
        --rendezvous http://198.51.100.1:8080 --stun 198.51.100.1:3478 \
        >"$work/$name-client.log" 2>&1; then
        cat "$work/$name-client.log" "$work/$name-host.log"
        echo "FAILED: $name: the session did not run"
        exit 1
    fi
    if ! grep -q "reached the host at .*($4)" "$work/$name-client.log"; then
        grep -E "reached|internet|relay" "$work/$name-client.log" || true
        echo "FAILED: $name: expected the path $4"
        exit 1
    fi
    grep "reached the host" "$work/$name-client.log"
    for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
    pids=()
    sleep 0.3
    for ns in wan hr h cr c; do ip netns del "fs-$ns"; done
}

run_case cone-cone cone cone Punched
run_case symmetric-client cone symmetric Relayed
run_case symmetric-both symmetric symmetric Relayed
echo "── all paths as expected"
