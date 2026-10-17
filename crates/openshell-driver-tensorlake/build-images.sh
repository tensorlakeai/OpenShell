#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build and register the sandbox image and the control-plane image on
# Tensorlake. A Tensorlake builder sandbox runs each build, so no local
# Docker is needed.
#
# The control image records the sandbox image name, so the two images always
# work as a pair.
#
# Optional:
#   VERSION         added to both names as `-<VERSION>` (for example 0-1-5)
#   SANDBOX_IMAGE   sandbox image name (default: openshell-tensorlake)
#   CONTROL_IMAGE   control image name (default: openshell-control)
#   PUBLIC=1        publish both images to all Tensorlake projects. The
#                   checkout must be clean, so the images match a commit.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/../.." && pwd)"
SUFFIX="${VERSION:+-${VERSION}}"
SANDBOX_IMAGE="${SANDBOX_IMAGE:-openshell-tensorlake}${SUFFIX}"
CONTROL_IMAGE="${CONTROL_IMAGE:-openshell-control}${SUFFIX}"
SOURCE_PATHS=(Cargo.toml Cargo.lock rust-toolchain.toml crates proto providers)

command -v tl >/dev/null || { echo "error: the tl CLI is not installed" >&2; exit 1; }

PUBLIC_FLAG=()
if [ "${PUBLIC:-}" = "1" ]; then
    if [ -n "$(git -C "${ROOT}" status --porcelain -- "${SOURCE_PATHS[@]}")" ]; then
        echo "error: PUBLIC=1 needs a clean checkout; commit or stash your changes" >&2
        exit 1
    fi
    PUBLIC_FLAG=(--public)
fi
COMMIT="$(git -C "${ROOT}" rev-parse HEAD)"
if [ -n "$(git -C "${ROOT}" status --porcelain -- "${SOURCE_PATHS[@]}")" ]; then
    COMMIT="${COMMIT}-dirty"
fi

CONTEXT="$(mktemp -d)"
trap 'rm -rf "${CONTEXT}"' EXIT

# Control-image build context: Dockerfile, start script, pairing data, and
# the Rust source (tracked and new files only, so build outputs stay out).
cp "${HERE}/control/Dockerfile" "${HERE}/control/openshell-control-start.sh" "${CONTEXT}/"
mkdir -p "${CONTEXT}/share" "${CONTEXT}/src"
printf '%s\n' "${SANDBOX_IMAGE}" >"${CONTEXT}/share/sandbox-image"
printf '%s\n' "${COMMIT}" >"${CONTEXT}/share/source-commit"
(
    cd "${ROOT}"
    git ls-files -z --cached --others --exclude-standard -- "${SOURCE_PATHS[@]}" \
        | while IFS= read -r -d '' file; do
            [ -e "${file}" ] && printf '%s\0' "${file}"
        done \
        | COPYFILE_DISABLE=1 tar --null -T - -cf - \
        | tar -xf - -C "${CONTEXT}/src"
)

echo "==> Building sandbox image ${SANDBOX_IMAGE}"
tl sbx image create "${HERE}/image/Dockerfile" \
    --registered-name "${SANDBOX_IMAGE}" \
    --docker_compat ${PUBLIC_FLAG[@]+"${PUBLIC_FLAG[@]}"}

echo "==> Building control image ${CONTROL_IMAGE} ($(du -sh "${CONTEXT}" | cut -f1) context)"
tl sbx image create "${CONTEXT}/Dockerfile" \
    --registered-name "${CONTROL_IMAGE}" \
    --docker_compat --cpus 8 --memory 16384 --builder_disk_mb 40960 \
    ${PUBLIC_FLAG[@]+"${PUBLIC_FLAG[@]}"}

cat <<EOF

Registered:
  sandbox image: ${SANDBOX_IMAGE}
  control image: ${CONTROL_IMAGE}
  source commit: ${COMMIT}

Deploy:
  CONTROL_IMAGE=${CONTROL_IMAGE} ${HERE#"${ROOT}/"}/control/deploy.sh
EOF
