#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Start the OpenShell control plane inside a Tensorlake sandbox.
#
# Run this script as the `openshell` user through a Tensorlake managed
# process with `--restart always`. The script:
#
# 1. Reads settings from /etc/openshell-control/control.env.
# 2. Creates the gateway signing keys on first start.
# 3. Starts the Tensorlake compute driver on a Unix socket.
# 4. Starts the gateway on port 8080 with plaintext HTTP/2. The Tensorlake
#    port proxy terminates TLS in front of it.
#
# The script exits when either process exits, so Tensorlake restarts both.

set -euo pipefail

ENV_FILE=/etc/openshell-control/control.env
STATE_DIR=/var/lib/openshell
BIN_DIR=/opt/openshell/bin
SOCKET="${STATE_DIR}/run/compute-driver.sock"
KEY_DIR="${STATE_DIR}/keys"
CONFIG="${STATE_DIR}/gateway.toml"
PUBLIC_CA=/etc/ssl/certs/ca-certificates.crt
PORT=8080

log() {
    printf 'openshell-control: %s\n' "$*" >&2
}

fail() {
    log "error: $*"
    exit 1
}

[ -r "${ENV_FILE}" ] || fail "cannot read ${ENV_FILE}"
set -a
# shellcheck source=/dev/null
. "${ENV_FILE}"
set +a

# Default to the sandbox image that was built with this control image.
DEFAULT_SANDBOX_IMAGE_FILE=/opt/openshell/share/sandbox-image
if [ -z "${OPENSHELL_SANDBOX_IMAGE:-}" ] && [ -r "${DEFAULT_SANDBOX_IMAGE_FILE}" ]; then
    OPENSHELL_SANDBOX_IMAGE="$(cat "${DEFAULT_SANDBOX_IMAGE_FILE}")"
fi

[ -n "${TENSORLAKE_API_KEY:-}" ] || fail "TENSORLAKE_API_KEY is not set"
[ -n "${OPENSHELL_CONTROL_PUBLIC_URL:-}" ] || fail "OPENSHELL_CONTROL_PUBLIC_URL is not set"
[ -n "${OPENSHELL_SANDBOX_IMAGE:-}" ] || fail "OPENSHELL_SANDBOX_IMAGE is not set"

# The public port accepts anonymous requests, because guards cannot hold a
# Tensorlake key. So users must authenticate to the gateway itself.
ALLOW_UNAUTHENTICATED=false
if [ -z "${OPENSHELL_OIDC_ISSUER:-}" ]; then
    if [ "${OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED:-}" != "1" ]; then
        fail "set OPENSHELL_OIDC_ISSUER, or set OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED=1 for a test gateway"
    fi
    log "WARNING: user authentication is off. Anyone with ${OPENSHELL_CONTROL_PUBLIC_URL} can control this gateway."
    ALLOW_UNAUTHENTICATED=true
fi

GATEWAY_NAME="${OPENSHELL_GATEWAY_NAME:-tensorlake}"

if [ ! -f "${KEY_DIR}/jwt/signing.pem" ]; then
    log "creating gateway signing keys"
    "${BIN_DIR}/openshell-gateway" generate-certs --output-dir "${KEY_DIR}"
fi

cat >"${CONFIG}" <<EOF
[openshell]
version = 2

[openshell.gateway]
name = "${GATEWAY_NAME}"

[openshell.gateway.auth]
allow_unauthenticated_users = ${ALLOW_UNAUTHENTICATED}

[openshell.gateway.gateway_jwt]
signing_key_path = "${KEY_DIR}/jwt/signing.pem"
public_key_path = "${KEY_DIR}/jwt/public.pem"
kid_path = "${KEY_DIR}/jwt/kid"
gateway_id = "${GATEWAY_NAME}"
EOF

install -d -m 0700 "$(dirname "${SOCKET}")"
rm -f "${SOCKET}"

DRIVER_PID=""
GATEWAY_PID=""
stop() {
    for pid in "${GATEWAY_PID}" "${DRIVER_PID}"; do
        if [ -n "${pid}" ]; then
            kill "${pid}" 2>/dev/null || true
        fi
    done
}
trap stop EXIT
trap 'exit 143' TERM INT

# The guards dial the public URL. They trust only the given CA bundle, so
# pass the public roots that sign the Tensorlake proxy certificate.
"${BIN_DIR}/openshell-driver-tensorlake" \
    --bind-socket "${SOCKET}" \
    --gateway-name "${GATEWAY_NAME}" \
    --grpc-endpoint "${OPENSHELL_CONTROL_PUBLIC_URL}" \
    --gateway-tls-ca "${PUBLIC_CA}" \
    --sandbox-image "${OPENSHELL_SANDBOX_IMAGE}" \
    --state-dir "${STATE_DIR}/tensorlake" &
DRIVER_PID=$!

for _ in $(seq 1 50); do
    [ -S "${SOCKET}" ] && break
    kill -0 "${DRIVER_PID}" 2>/dev/null || fail "compute driver exited during start"
    sleep 0.2
done
[ -S "${SOCKET}" ] || fail "compute driver did not create ${SOCKET}"

# The gateway does not need the Tensorlake key.
env -u TENSORLAKE_API_KEY "${BIN_DIR}/openshell-gateway" \
    --config "${CONFIG}" \
    --bind-address 0.0.0.0 \
    --port "${PORT}" \
    --disable-tls \
    --compute-driver tensorlake \
    --compute-driver-socket "${SOCKET}" \
    --db-url "${OPENSHELL_DB_URL:-sqlite:${STATE_DIR}/gateway.db?mode=rwc}" &
GATEWAY_PID=$!

log "control plane started at ${OPENSHELL_CONTROL_PUBLIC_URL}"

set +e
wait -n "${DRIVER_PID}" "${GATEWAY_PID}"
status=$?
log "a control-plane process exited with status ${status}"
exit "${status}"
