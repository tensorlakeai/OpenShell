#!/bin/bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Build and register the sandbox image and the control-plane image on
# Tensorlake as CAS images. Tensorlake runs each build, so no local Docker is
# needed. CAS images start fast, because content streams in on demand.
#
# The control image records the sandbox image name, so the two images always
# work as a pair.
#
# Optional:
#   PUBLIC=1        publish both images to all Tensorlake projects. The
#                   checkout must be clean, so the images match a commit.
#   VERSION         added to the default names as `-<VERSION>`. A public
#                   build without VERSION takes it from the checkout: the
#                   release tag on HEAD with dots turned into dashes
#                   (v0.1.6-pre.2 -> 0-1-6-pre-2), or else the short commit
#                   hash. So a published pair is named after what it holds.
#   SANDBOX_IMAGE   full sandbox image name (default: openshell-tensorlake[-VERSION])
#   CONTROL_IMAGE   full control image name (default: openshell-control[-VERSION])

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${HERE}/../.." && pwd)"
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

# A public build is named after its source, so no two publishes share a
# name and a reader can tell what an image holds. Tensorlake has no image
# tags, so the version is part of the name.
if [ -z "${VERSION:-}" ] && [ "${PUBLIC:-}" = "1" ]; then
    if TAG="$(git -C "${ROOT}" describe --tags --exact-match --match 'v[0-9]*' HEAD 2>/dev/null)"; then
        VERSION="${TAG#v}"
        VERSION="${VERSION//./-}"
    else
        VERSION="$(git -C "${ROOT}" rev-parse --short=7 HEAD)"
    fi
fi
if [ -n "${VERSION:-}" ] && ! [[ "${VERSION}" =~ ^[a-z0-9][a-z0-9-]*$ ]]; then
    echo "error: VERSION must contain only lowercase letters, digits, and dashes: ${VERSION}" >&2
    exit 1
fi
# VERSION changes only the default names. Names set in SANDBOX_IMAGE or
# CONTROL_IMAGE are used as they are.
SUFFIX="${VERSION:+-${VERSION}}"
SANDBOX_IMAGE="${SANDBOX_IMAGE:-openshell-tensorlake${SUFFIX}}"
CONTROL_IMAGE="${CONTROL_IMAGE:-openshell-control${SUFFIX}}"

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
    --cas ${PUBLIC_FLAG[@]+"${PUBLIC_FLAG[@]}"}

echo "==> Building control image ${CONTROL_IMAGE} ($(du -sh "${CONTEXT}" | cut -f1) context)"
tl sbx image create "${CONTEXT}/Dockerfile" \
    --registered-name "${CONTROL_IMAGE}" \
    --cas --cpus 8 --memory 16384 --builder_disk_mb 40960 \
    ${PUBLIC_FLAG[@]+"${PUBLIC_FLAG[@]}"}

cat <<EOF

Registered:
  sandbox image: ${SANDBOX_IMAGE}
  control image: ${CONTROL_IMAGE}
  version: ${VERSION:-(none)}
  source commit: ${COMMIT}

Deploy:
  CONTROL_IMAGE=${CONTROL_IMAGE} ${HERE#"${ROOT}/"}/control/deploy.sh
EOF
