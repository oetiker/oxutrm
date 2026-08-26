#!/bin/sh
# Build a NAT topology out of Linux network namespaces, run a command inside
# it, and tear the whole thing down again.
#
# This is the only honest way to test rungs 1-3. Everything else in this crate
# runs on loopback, where THERE IS NO NAT TO TRAVERSE — every rung could be
# subtly broken and every unit test would still pass.
#
# It runs UNPRIVILEGED. `unshare --user --map-root-user` makes us root inside a
# fresh user namespace, which is enough for veth, addresses, routes and nft,
# and gives the host no new capabilities at all. Namespaces are held open by
# child `sleep` processes rather than `ip netns add`, which would need to write
# to /var/run/netns and therefore real root.
#
# WHAT THE SYMMETRIC TOPOLOGY IS, AND IS NOT
# ------------------------------------------
# `masquerade fully-random` varies the external port per destination, which
# exercises the same failure mode and the same rung-3 recovery as a commercial
# symmetric NAT. It is an APPROXIMATION. Linux cannot reproduce every NAT in
# the field, and these tests prove the RECOVERY PATH, not universal
# compatibility. Claim the former; do not claim the latter.
#
# Note the spelling: `fully-random` is nftables. `random-fully` is iptables and
# nft rejects it. Getting that wrong leaves an ordinary cone NAT, so the NAT
# typing never reports Symmetric, rung 3 never runs, and the test passes while
# proving nothing. The Rust test therefore asserts the topology really is
# symmetric BEFORE asserting the blast works.
#
# Usage:  topology.sh <cone|symmetric|double> -- <command...>
#
# The command runs in the CLIENT namespace. These variables are exported:
#   OXUTRM_STUN        primary STUN server address     (10.0.2.2:3478)
#   OXUTRM_STUN_ALT    second port, same IP, for probe 2 (10.0.2.2:3479)
#   OXUTRM_STUN2       second server, different IP, probe 3 (10.0.2.3:3478)
#   OXUTRM_PEER_NS     pid of the peer namespace, for nsenter
#   OXUTRM_TOPOLOGY    the topology name

set -eu

TOPOLOGY="${1:?usage: topology.sh <cone|symmetric|double> -- <command...>}"
shift
[ "${1:-}" = "--" ] && shift

# Re-exec ourselves inside a fresh user+net+mount namespace, as root within it.
if [ "${OXUTRM_NETNS_INNER:-}" != "1" ]; then
    OXUTRM_NETNS_INNER=1
    export OXUTRM_NETNS_INNER
    # `--pid --mount-proc` is a LEAK FIX, not tidiness. `cleanup` below kills
    # the pids it recorded, but `nsx` is a shell function, so `nsx ... &`
    # backgrounds a SUBSHELL and `$!` names that subshell rather than the
    # responder it goes on to run. Killing it orphaned the STUN responders to
    # init on every ordinary run; 448 of them accumulated over one day. A trap
    # cannot fix that, and cannot run at all when the harness is SIGKILLed by
    # the OOM killer, which is the other way they escaped.
    #
    # With `--pid`, this script becomes pid 1 of a new pid namespace, and the
    # kernel SIGKILLs every process in it when pid 1 exits — grandchildren
    # included, and however pid 1 died. `cleanup` stays as the tidy path; this
    # is the backstop underneath it. `--mount-proc` is required so that /proc
    # reflects the new namespace rather than the host's.
    exec unshare --user --map-root-user --net --mount --pid --mount-proc --fork \
        "$0" "$TOPOLOGY" -- "$@"
fi

PIDS=""

cleanup() {
    # Killing the holder processes releases their namespaces, and every veth
    # inside them goes with it. Nothing outside this process tree was touched,
    # so there is nothing else to undo.
    for p in $PIDS; do
        kill "$p" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

# Start a process that holds an empty network namespace open. Echoes its pid.
new_ns() {
    # stdio goes to /dev/null: a background holder that inherited our pipe
    # would keep it open after we exit, and any caller reading to EOF would
    # hang forever waiting for a `sleep` to finish.
    unshare --net sleep 3600 </dev/null >/dev/null 2>&1 &
    _pid=$!
    # Wait for the namespace to actually exist before anything is moved into it.
    _tries=0
    while [ ! -e "/proc/$_pid/ns/net" ]; do
        _tries=$((_tries + 1))
        [ "$_tries" -gt 200 ] && { echo "namespace $_pid never appeared" >&2; exit 1; }
        sleep 0.01
    done
    PIDS="$PIDS $_pid"
    echo "$_pid"
}

nsx() { _p=$1; shift; nsenter --net --target "$_p" "$@"; }

# A veth pair with one end in $1 and the other in $2, addressed and up.
#   link <pid-a> <ifname-a> <cidr-a> <pid-b> <ifname-b> <cidr-b>
link() {
    ip link add "$2" type veth peer name "$5"
    ip link set "$2" netns "$1"
    ip link set "$5" netns "$4"
    nsx "$1" ip addr add "$3" dev "$2"
    nsx "$1" ip link set "$2" up
    nsx "$1" ip link set lo up
    nsx "$4" ip addr add "$6" dev "$5"
    nsx "$4" ip link set "$5" up
    nsx "$4" ip link set lo up
}

# Turn a namespace into a NAT router.
#   nat <pid> <outside-ifname> [fully-random]
nat() {
    _p=$1; _oif=$2; _mode=${3:-}
    nsx "$_p" sysctl -q -w net.ipv4.ip_forward=1
    nsx "$_p" nft add table ip nat
    nsx "$_p" nft add chain ip nat post '{ type nat hook postrouting priority 100 ; }'
    if [ -n "$_mode" ]; then
        # Varies the external port per destination: a symmetric mapping, which
        # is the case rung 3 exists for.
        nsx "$_p" nft add rule ip nat post oifname "$_oif" masquerade fully-random
    else
        # Linux conntrack's default is endpoint-independent mapping with
        # address-and-port-dependent filtering — exactly a port-restricted cone.
        nsx "$_p" nft add rule ip nat post oifname "$_oif" masquerade
    fi
}

INET=$(new_ns)     # the "internet": STUN responders and the far peer live here
NAT1=$(new_ns)     # the client's NAT
CLIENT=$(new_ns)   # the client, behind NAT1

case "$TOPOLOGY" in
cone|symmetric)
    link "$CLIENT" cli0 10.0.1.2/24 "$NAT1" nat0 10.0.1.1/24
    link "$NAT1"   nat1 10.0.2.1/24 "$INET" inet0 10.0.2.2/24
    nsx "$CLIENT" ip route add default via 10.0.1.1
    # DELIBERATELY no route from the internet side into 10.0.1.0/24. With one,
    # the far peer can dial the client's private address directly and the NAT
    # is never traversed at all — the test would pass while proving only that
    # routing works. The only way in is the mapping the client's own outbound
    # packet creates.
    # A second address on the same host, so probe 3 has a genuinely different
    # server IP to compare against. Without it NAT typing can never conclude.
    nsx "$INET" ip addr add 10.0.2.3/24 dev inet0
    if [ "$TOPOLOGY" = symmetric ]; then
        nat "$NAT1" nat1 fully-random
    else
        nat "$NAT1" nat1
    fi
    ;;
double)
    # Two nested NATs: rung 1 must fail cleanly (no router will map for us
    # through two layers) and rung 2 must take over.
    NAT2=$(new_ns)
    link "$CLIENT" cli0 10.0.1.2/24 "$NAT1" nat0 10.0.1.1/24
    link "$NAT1"   nat1 10.0.3.2/24 "$NAT2" nat2 10.0.3.1/24
    link "$NAT2"   nat3 10.0.2.1/24 "$INET" inet0 10.0.2.2/24
    nsx "$CLIENT" ip route add default via 10.0.1.1
    nsx "$NAT1"   ip route add default via 10.0.3.1
    # Again, no route inward: both layers must be traversed, not bypassed.
    nsx "$INET" ip addr add 10.0.2.3/24 dev inet0
    nat "$NAT1" nat1
    nat "$NAT2" nat3
    ;;
*)
    echo "unknown topology: $TOPOLOGY" >&2
    exit 2
    ;;
esac

# The isolated namespaces have no route to any public STUN server, so the
# harness hosts its own. Without this rungs 2 and 3 cannot be exercised at all.
STUN_BIN="${OXUTRM_NETNS_PEER:?OXUTRM_NETNS_PEER must name the helper binary}"
for _bind in 10.0.2.2:3478 10.0.2.2:3479 10.0.2.3:3478; do
    nsx "$INET" "$STUN_BIN" stun --bind "$_bind" </dev/null >/dev/null 2>&1 &
    PIDS="$PIDS $!"
done

# Give the responders a moment to bind before anything queries them.
sleep 0.3

OXUTRM_STUN=10.0.2.2:3478
OXUTRM_STUN_ALT=10.0.2.2:3479
OXUTRM_STUN2=10.0.2.3:3478
OXUTRM_PEER_NS="$INET"
OXUTRM_TOPOLOGY="$TOPOLOGY"
export OXUTRM_STUN OXUTRM_STUN_ALT OXUTRM_STUN2 OXUTRM_PEER_NS OXUTRM_TOPOLOGY

# An optional peer, launched on the internet side before the client runs.
# This is what makes an end-to-end traversal test possible: one side behind
# the NAT, one side reachable, exactly the asymmetry the ladder must cross.
if [ -n "${OXUTRM_PEER_CMD:-}" ]; then
    # shellcheck disable=SC2086
    nsx "$INET" $OXUTRM_PEER_CMD >"${OXUTRM_PEER_LOG:-/dev/null}" 2>&1 </dev/null &
    PIDS="$PIDS $!"
    sleep 0.3
fi

nsx "$CLIENT" "$@"
