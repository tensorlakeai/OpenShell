#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Start OpenShell inside a Tensorlake sandbox.
#
# The Tensorlake compute driver uploads a launch bundle and runs this script
# as root through the Tensorlake process API. The script:
#
# 1. Extracts the bundle with the owners and modes the driver set, then
#    deletes it.
# 2. Starts the workload runtime (openshell-sandbox) in a new network
#    namespace that has only loopback. The workload has no network path
#    except the authenticated boundary channel to the guard.
# 3. Starts the guard (openshell-supervisor) as an unprivileged identity
#    that the workload cannot read.
#
# The script stays in the foreground and exits when either process exits.

set -euo pipefail

BUNDLE="${1:-/var/lib/openshell/launch-bundle.tar}"
BIN_DIR=/opt/openshell/bin
PARAMS=/.openshell/launch/params
SUPERVISOR_ENV=/.openshell/supervisor/env
SUPERVISOR_ARGS=/.openshell/supervisor/args
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
WORKLOAD_UID="" WORKLOAD_GID="" SUPERVISOR_UID="" SUPERVISOR_GID=""
WORKSPACE="" SUPERVISOR_RUN_DIR=""
while IFS='=' read -r key value; do
    case "$key" in
        WORKLOAD_UID) WORKLOAD_UID="$value" ;;
        WORKLOAD_GID) WORKLOAD_GID="$value" ;;
        SUPERVISOR_UID) SUPERVISOR_UID="$value" ;;
        SUPERVISOR_GID) SUPERVISOR_GID="$value" ;;
        WORKSPACE) WORKSPACE="$value" ;;
        SUPERVISOR_RUN_DIR) SUPERVISOR_RUN_DIR="$value" ;;
        "") ;;
        *) fatal "unknown launch parameter $key" ;;
    esac
done <"$PARAMS"
for id in "$WORKLOAD_UID" "$WORKLOAD_GID" "$SUPERVISOR_UID" "$SUPERVISOR_GID"; do
    [[ "$id" =~ ^[0-9]+$ ]] || fatal "launch identity '$id' is not numeric"
done
[[ "$WORKSPACE" == /* && "$SUPERVISOR_RUN_DIR" == /* ]] || fatal "launch paths must be absolute"

# Guard runtime directory: proxy TLS material, SSH socket, health socket.
install -d -m 0700 -o "$SUPERVISOR_UID" -g "$SUPERVISOR_GID" "$SUPERVISOR_RUN_DIR"

# Workload directories. openshell-sandbox makes the workspace owned by the
# workload identity before it drops privileges.
install -d -m 0755 "$WORKSPACE"
# Public supervisor CA for workload TLS trust. openshell-sandbox requires the
# parent to be traversable by every workload identity, and creates the
# `material` child as the workload identity. It holds no secrets.
install -d -m 0755 -o "$WORKLOAD_UID" -g "$WORKLOAD_GID" /run/openshell-supervisor-ca
printf 'nameserver 127.0.0.53\noptions timeout:2 attempts:2\n' >"$WORKLOAD_RESOLV_CONF"
chmod 0644 "$WORKLOAD_RESOLV_CONF"

pids=()
cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

# Workload: private network and mount namespaces. Only loopback exists in
# the network namespace. The resolver points at the sandbox DNS relay.
log "starting workload runtime as ${WORKLOAD_UID}:${WORKLOAD_GID}"
unshare --net --mount --propagation private -- /bin/bash -euc '
    ip link set lo up
    echo 0 >/proc/sys/net/ipv4/ip_unprivileged_port_start
    mount --bind "$1" /etc/resolv.conf
    exec "$2/openshell-sandbox" launch-capability-free "$3" "$4" "$5" "$6"
' openshell-workload "$WORKLOAD_RESOLV_CONF" "$BIN_DIR" \
    "$WORKLOAD_UID" "$WORKLOAD_GID" "$BOOTSTRAP" "$WORKSPACE" &
pids+=("$!")

# Guard: unprivileged identity, clean environment, no capabilities.
mapfile -d '' -t supervisor_env <"$SUPERVISOR_ENV"
mapfile -d '' -t supervisor_args <"$SUPERVISOR_ARGS"
log "starting guard as ${SUPERVISOR_UID}:${SUPERVISOR_GID}"
setpriv --reuid="$SUPERVISOR_UID" --regid="$SUPERVISOR_GID" --clear-groups \
    --inh-caps=-all --bounding-set=-all --no-new-privs \
    env -i "${supervisor_env[@]}" "$BIN_DIR/openshell-supervisor" "${supervisor_args[@]}" &
pids+=("$!")

set +e
wait -n "${pids[@]}"
status=$?
set -e
log "a launched process exited with status $status; stopping the other"
exit "$status"
