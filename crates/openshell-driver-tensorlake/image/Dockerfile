# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Tensorlake sandbox image for the OpenShell Tensorlake compute driver.
#
# It adds the OpenShell guard (openshell-supervisor), the workload runtime
# (openshell-sandbox), and the launcher to an Ubuntu base. Add agent tools
# (for example Claude Code) in a derived image.
#
# Build and register it on Tensorlake with ../build-images.sh (no local
# Docker needed).
#
# Tensorlake uses this directory as the build context, and does not expand
# variables in FROM, so the image tags are fixed here. Pin the upstream
# commit this branch is based on, the same as control/Dockerfile.

FROM ghcr.io/nvidia/openshell/supervisor:a4c848adbddd0b4732fd70ddcf8a563a5b1e9317 AS supervisor
FROM ghcr.io/nvidia/openshell/sandbox:a4c848adbddd0b4732fd70ddcf8a563a5b1e9317 AS sandbox

FROM ubuntu:24.04

# iproute2: loopback in the workload network namespace.
# util-linux: unshare and setpriv. ca-certificates: guard TLS to upstreams.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates iproute2 util-linux \
    && rm -rf /var/lib/apt/lists/*

# Default workload identity (1000:1000), as in other OpenShell drivers.
RUN if ! getent group 1000 >/dev/null; then groupadd --gid 1000 sandbox; fi \
    && if ! getent passwd 1000 >/dev/null; then \
         useradd --uid 1000 --gid 1000 --home-dir /sandbox --no-create-home --shell /bin/bash sandbox; \
       fi \
    && install -d -m 0755 /sandbox /var/lib/openshell

COPY --from=supervisor /openshell-supervisor /opt/openshell/bin/openshell-supervisor
COPY --from=sandbox /openshell-sandbox /opt/openshell/bin/openshell-sandbox
COPY openshell-tensorlake-launch.sh /opt/openshell/bin/openshell-tensorlake-launch
RUN chmod 0555 /opt/openshell/bin/openshell-supervisor /opt/openshell/bin/openshell-sandbox \
    /opt/openshell/bin/openshell-tensorlake-launch

WORKDIR /sandbox
