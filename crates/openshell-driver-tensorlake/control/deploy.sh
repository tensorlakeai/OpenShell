#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Create the OpenShell control-plane sandbox on Tensorlake and start it.
#
# Run this on your machine with the `tl` CLI logged in. It is safe to run
# again: it restarts the control plane with new settings and keeps the
# gateway state.
#
# Required:
#   TENSORLAKE_API_KEY            key the driver uses to create sandboxes
# Optional:
#   CONTROL_NAME                  sandbox name (default: openshell-control)
#   CONTROL_IMAGE                 registered control-plane image (default: openshell-control)
#   OPENSHELL_SANDBOX_IMAGE       sandbox image (default: the one built with CONTROL_IMAGE)
#   OPENSHELL_OIDC_ISSUER, OPENSHELL_OIDC_AUDIENCE, ...
#                                 gateway OIDC settings for user authentication
#   OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED=1
#                                 test only: turn off user authentication
#   OPENSHELL_TENSORLAKE_ALLOW_OUT
#                                 comma-separated egress allow list for sandboxes
#   OPENSHELL_DB_URL              external database, for example Postgres

set -euo pipefail

NAME="${CONTROL_NAME:-openshell-control}"
IMAGE="${CONTROL_IMAGE:-openshell-control}"
PORT=8080
PUBLIC_URL="https://${PORT}-${NAME}.sandbox.tensorlake.ai"
PROCESS=openshell-control
REMOTE_ENV=/etc/openshell-control/control.env

[ -n "${TENSORLAKE_API_KEY:-}" ] || { echo "error: set TENSORLAKE_API_KEY" >&2; exit 1; }
command -v tl >/dev/null || { echo "error: the tl CLI is not installed" >&2; exit 1; }

if tl sbx describe "${NAME}" >/dev/null 2>&1; then
    echo "==> Using existing sandbox ${NAME}"
    tl sbx kill "${NAME}" "${PROCESS}" >/dev/null 2>&1 || true
else
    echo "==> Creating sandbox ${NAME} from image ${IMAGE}"
    tl sbx create "${NAME}" --image "${IMAGE}" --cpus 2 --memory 4096
fi

# Guards in other sandboxes dial this port without a Tensorlake key.
echo "==> Exposing port ${PORT}"
tl sbx port expose "${NAME}" "${PORT}"

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT
ENV_FILE="${TMP_DIR}/control.env"
(
    umask 077
    {
        printf 'TENSORLAKE_API_KEY=%q\n' "${TENSORLAKE_API_KEY}"
        printf 'OPENSHELL_CONTROL_PUBLIC_URL=%q\n' "${PUBLIC_URL}"
        for var in $(compgen -e); do
            case "${var}" in
                OPENSHELL_OIDC_* | OPENSHELL_TENSORLAKE_* | OPENSHELL_DB_URL | OPENSHELL_SANDBOX_IMAGE \
                    | OPENSHELL_GATEWAY_NAME | OPENSHELL_CONTROL_ALLOW_UNAUTHENTICATED)
                    printf '%s=%q\n' "${var}" "${!var}"
                    ;;
            esac
        done
    } >"${ENV_FILE}"
)

echo "==> Uploading settings"
tl sbx cp "${ENV_FILE}" "${NAME}:${REMOTE_ENV}"
tl sbx exec --user root "${NAME}" /bin/sh -c \
    "chown openshell:openshell ${REMOTE_ENV} && chmod 0600 ${REMOTE_ENV} \
     && chown -R openshell:openshell /var/lib/openshell /etc/openshell-control"

echo "==> Starting the control plane"
tl sbx exec --detach --name "${PROCESS}" --restart always --user openshell \
    "${NAME}" /opt/openshell/bin/openshell-control-start

cat <<EOF

Control plane: ${PUBLIC_URL}

Logs:
  tl sbx logs ${NAME}

Connect the openshell CLI through an authenticated Tensorlake tunnel:
  tl sbx tunnel ${NAME} ${PORT} --listen-port 18080
  openshell gateway add http://127.0.0.1:18080 --name tensorlake
EOF
