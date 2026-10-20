#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Start OpenShell inside a Tensorlake sandbox.
#
# The Tensorlake compute driver uploads a launch bundle and runs this script
# as root through the Tensorlake process API. The bundle's launch parameters
# name the role this VM plays:
#
#   workload  Start the workload runtime (openshell-sandbox) in a network
#             namespace that has loopback and one veth link to this VM. It
#             listens for the guard on a TLS port. A forwarder in the VM's
#             own namespace accepts the Tensorlake tunnel on loopback and
#             connects into the workload namespace over the veth link, so
#             the runtime sees a non-loopback peer (it rejects loopback
#             peers). Tensorlake's network policy gives this VM no outbound
#             path; that policy and the namespace are the outer fence.
#   guard     Start a `tl sbx tunnel` to the workload VM and the guard
#             (openshell-supervisor), as two unprivileged identities that
#             cannot read each other's files.
#
# The script extracts the bundle with the owners and modes the driver set,
# deletes it, starts the role's processes, and stays in the foreground. It
# exits when any process it started exits.

set -euo pipefail

BUNDLE="${1:-/var/lib/openshell/launch-bundle.tar}"
BIN_DIR=/opt/openshell/bin
PARAMS=/.openshell/launch/params
SUPERVISOR_ENV=/.openshell/supervisor/env
SUPERVISOR_ARGS=/.openshell/supervisor/args
TUNNEL_ENV=/.openshell/tunnel/env
TUNNEL_ARGS=/.openshell/tunnel/args
WORKLOAD_NETNS=openshell-workload
# Link-local addresses for the veth link between the VM and the workload
# namespace. The forwarder connects from .1 to the runtime at .2.
VETH_HOST_ADDR=169.254.77.1
VETH_WORKLOAD_ADDR=169.254.77.2
BOOTSTRAP=/.openshell/channel/sandbox/bootstrap.json
WORKLOAD_RESOLV_CONF=/.openshell/launch/resolv.conf

log() {
    printf '[openshell-tensorlake-launch] %s\n' "$*" >&2
}

fatal() {
    log "FATAL: $*"
    exit 1
}

[ "$(id -u)" -eq 0 ] || fatal "must run as root"
[ -f "$BUNDLE" ] || fatal "launch bundle $BUNDLE is missing"

umask 077
chmod 0600 "$BUNDLE"
if [ -e /.openshell ]; then
    fatal "/.openshell already exists; this sandbox was already launched"
fi
tar --extract --file "$BUNDLE" --directory / --same-owner --same-permissions --numeric-owner
rm -f "$BUNDLE"

# Read launch parameters (KEY=VALUE, numeric values and absolute paths).
ROLE=""
WORKLOAD_UID="" WORKLOAD_GID="" SUPERVISOR_UID="" SUPERVISOR_GID="" TUNNEL_UID="" TUNNEL_GID=""
FORWARDER_UID="" FORWARDER_GID="" WORKLOAD_LISTEN_PORT="" WORKLOAD_FORWARD_PORT=""
WORKSPACE="" SUPERVISOR_RUN_DIR="" TUNNEL_RUN_DIR=""
while IFS='=' read -r key value; do
    case "$key" in
        ROLE) ROLE="$value" ;;
        WORKLOAD_UID) WORKLOAD_UID="$value" ;;
        WORKLOAD_GID) WORKLOAD_GID="$value" ;;
        SUPERVISOR_UID) SUPERVISOR_UID="$value" ;;
        SUPERVISOR_GID) SUPERVISOR_GID="$value" ;;
        TUNNEL_UID) TUNNEL_UID="$value" ;;
        TUNNEL_GID) TUNNEL_GID="$value" ;;
        FORWARDER_UID) FORWARDER_UID="$value" ;;
        FORWARDER_GID) FORWARDER_GID="$value" ;;
        WORKLOAD_LISTEN_PORT) WORKLOAD_LISTEN_PORT="$value" ;;
        WORKLOAD_FORWARD_PORT) WORKLOAD_FORWARD_PORT="$value" ;;
        WORKSPACE) WORKSPACE="$value" ;;
        SUPERVISOR_RUN_DIR) SUPERVISOR_RUN_DIR="$value" ;;
        TUNNEL_RUN_DIR) TUNNEL_RUN_DIR="$value" ;;
        "") ;;
        *) fatal "unknown launch parameter $key" ;;
    esac
done <"$PARAMS"

require_numeric() {
    for id in "$@"; do
        [[ "$id" =~ ^[0-9]+$ ]] || fatal "launch identity '$id' is not numeric"
    done
}

require_absolute() {
    for path in "$@"; do
        [[ "$path" == /* ]] || fatal "launch path '$path' must be absolute"
    done
}

pids=()
cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

# Workload runtime, in private network, mount, and PID namespaces. The
# network namespace has loopback and one veth link to this VM, over which
# the guard connects through the forwarder. The resolver points at the
# sandbox runtime's DNS relay, and unprivileged processes may bind low ports
# so the relay can take port 53.
#
# The runtime is PID 1 of its own PID namespace, with a procfs that shows
# only that namespace. Process identity resolution for network policy walks
# a connecting process's ancestors up to the namespace init; without the
# namespace it would reach this root-owned script and fail.
start_workload() {
    require_numeric "$WORKLOAD_UID" "$WORKLOAD_GID"
    require_absolute "$WORKSPACE"
    install -d -m 0755 "$WORKSPACE"
    # Public supervisor CA for workload TLS trust. openshell-sandbox requires
    # the parent to be traversable by every workload identity, and creates
    # the `material` child as the workload identity. It holds no secrets.
    install -d -m 0755 -o "$WORKLOAD_UID" -g "$WORKLOAD_GID" /run/openshell-supervisor-ca
    printf 'nameserver 127.0.0.53\noptions timeout:2 attempts:2\n' >"$WORKLOAD_RESOLV_CONF"
    chmod 0644 "$WORKLOAD_RESOLV_CONF"
    ip netns add "$WORKLOAD_NETNS"
    ip link add osfw0 type veth peer name osfw1
    ip link set osfw1 netns "$WORKLOAD_NETNS"
    ip addr add "$VETH_HOST_ADDR/30" dev osfw0
    ip link set osfw0 up
    ip -n "$WORKLOAD_NETNS" addr add "$VETH_WORKLOAD_ADDR/30" dev osfw1
    ip -n "$WORKLOAD_NETNS" link set osfw1 up
    ip -n "$WORKLOAD_NETNS" link set lo up
    log "starting workload runtime as ${WORKLOAD_UID}:${WORKLOAD_GID}"
    ip netns exec "$WORKLOAD_NETNS" unshare --pid --fork --kill-child --mount-proc \
        --mount --propagation private -- /bin/bash -euc '
        ip link set lo up
        echo 0 >/proc/sys/net/ipv4/ip_unprivileged_port_start
        mount --bind "$1" /etc/resolv.conf
        exec "$2/openshell-sandbox" launch-capability-free "$3" "$4" "$5" "$6"
    ' openshell-workload "$WORKLOAD_RESOLV_CONF" "$BIN_DIR" \
        "$WORKLOAD_UID" "$WORKLOAD_GID" "$BOOTSTRAP" "$WORKSPACE" &
    pids+=("$!")
}

# Forwarder for the guard's tunnel. The Tensorlake tunnel delivers to
# 127.0.0.1 in this VM, and the sandbox runtime rejects loopback peers, so
# the forwarder accepts on loopback and connects into the workload namespace
# from the veth address. It runs as an unprivileged identity with no files
# of its own. Every boundary RPC crosses both sockets as small frames, so
# both disable Nagle (nodelay) to avoid delayed-ACK stalls.
start_forwarder() {
    require_numeric "$FORWARDER_UID" "$FORWARDER_GID" "$WORKLOAD_LISTEN_PORT" "$WORKLOAD_FORWARD_PORT"
    log "starting tunnel forwarder 127.0.0.1:${WORKLOAD_FORWARD_PORT} -> ${VETH_WORKLOAD_ADDR}:${WORKLOAD_LISTEN_PORT}"
    setpriv --reuid="$FORWARDER_UID" --regid="$FORWARDER_GID" --clear-groups \
        --inh-caps=-all --bounding-set=-all --no-new-privs \
        env -i PATH=/usr/bin:/bin socat \
        "TCP4-LISTEN:${WORKLOAD_FORWARD_PORT},bind=127.0.0.1,reuseaddr,fork,nodelay" \
        "TCP4:${VETH_WORKLOAD_ADDR}:${WORKLOAD_LISTEN_PORT},bind=${VETH_HOST_ADDR},nodelay" &
    pids+=("$!")
}

# Guard: unprivileged identity, clean environment, no capabilities.
start_guard() {
    require_numeric "$SUPERVISOR_UID" "$SUPERVISOR_GID"
    require_absolute "$SUPERVISOR_RUN_DIR"
    # Guard runtime directory: proxy TLS material, SSH socket, health socket.
    install -d -m 0700 -o "$SUPERVISOR_UID" -g "$SUPERVISOR_GID" "$SUPERVISOR_RUN_DIR"
    local supervisor_env supervisor_args
    mapfile -d '' -t supervisor_env <"$SUPERVISOR_ENV"
    mapfile -d '' -t supervisor_args <"$SUPERVISOR_ARGS"
    log "starting guard as ${SUPERVISOR_UID}:${SUPERVISOR_GID}"
    setpriv --reuid="$SUPERVISOR_UID" --regid="$SUPERVISOR_GID" --clear-groups \
        --inh-caps=-all --bounding-set=-all --no-new-privs \
        env -i "${supervisor_env[@]}" "$BIN_DIR/openshell-supervisor" "${supervisor_args[@]}" &
    pids+=("$!")
}

# Tunnel to the workload VM. The tunnel identity holds the Tensorlake key
# and nothing else; the guard identity cannot read it.
start_tunnel() {
    require_numeric "$TUNNEL_UID" "$TUNNEL_GID"
    require_absolute "$TUNNEL_RUN_DIR"
    install -d -m 0700 -o "$TUNNEL_UID" -g "$TUNNEL_GID" "$TUNNEL_RUN_DIR"
    local tunnel_env tunnel_args
    mapfile -d '' -t tunnel_env <"$TUNNEL_ENV"
    mapfile -d '' -t tunnel_args <"$TUNNEL_ARGS"
    log "starting tunnel to the workload VM as ${TUNNEL_UID}:${TUNNEL_GID}"
    setpriv --reuid="$TUNNEL_UID" --regid="$TUNNEL_GID" --clear-groups \
        --inh-caps=-all --bounding-set=-all --no-new-privs \
        env -i HOME="$TUNNEL_RUN_DIR" PATH=/usr/bin:/bin "${tunnel_env[@]}" \
        "$BIN_DIR/tl" "${tunnel_args[@]}" &
    pids+=("$!")
}

case "$ROLE" in
    workload)
        start_workload
        start_forwarder
        ;;
    guard)
        start_tunnel
        start_guard
        ;;
    *) fatal "unknown launch role '$ROLE'" ;;
esac

set +e
wait -n "${pids[@]}"
status=$?
set -e
log "a launched process exited with status $status; stopping the others"
exit "$status"
