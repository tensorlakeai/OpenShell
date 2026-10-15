# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Tensorlake sandbox image for the OpenShell Tensorlake compute driver.
#
# It adds the OpenShell guard (openshell-supervisor), the workload runtime
# (openshell-sandbox), and the launcher to an Ubuntu base. Add agent tools
# (for example Claude Code) in a derived image.
#
# Build from the repository root:
#   docker build -f crates/openshell-driver-tensorlake/image/Dockerfile \
#     --build-arg OPENSHELL_TAG=<release tag> -t openshell-tensorlake .
# Then register the image with Tensorlake (`tl sbx image create`).

ARG OPENSHELL_TAG=latest
ARG BASE_IMAGE=ubuntu:24.04

FROM ghcr.io/nvidia/openshell/supervisor:${OPENSHELL_TAG} AS supervisor
FROM ghcr.io/nvidia/openshell/sandbox:${OPENSHELL_TAG} AS sandbox

FROM ${BASE_IMAGE}

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

COPY --from=supervisor --chmod=0555 /openshell-supervisor /opt/openshell/bin/openshell-supervisor
COPY --from=sandbox --chmod=0555 /openshell-sandbox /opt/openshell/bin/openshell-sandbox
COPY --chmod=0555 crates/openshell-driver-tensorlake/image/openshell-tensorlake-launch.sh \
    /opt/openshell/bin/openshell-tensorlake-launch

WORKDIR /sandbox
