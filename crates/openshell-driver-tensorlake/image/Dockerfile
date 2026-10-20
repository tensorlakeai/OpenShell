# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Tensorlake sandbox image for the OpenShell Tensorlake compute driver.
#
# It adds the OpenShell guard (openshell-supervisor), the workload runtime
# (openshell-sandbox), the Tensorlake CLI for the guard's tunnel, and the
# launcher to an Ubuntu base. One image serves both VMs of a sandbox: the
# workload VM runs openshell-sandbox from it, the guard VM runs the tunnel
# and openshell-supervisor from it. Add agent tools (for example Claude
# Code) in a derived image; the guard VM ignores them.
#
# Build and register it on Tensorlake with ../build-images.sh (no local
# Docker needed).
#
# Tensorlake uses this directory as the build context, and does not expand
# variables in FROM, so the image tags are fixed here. Pin the upstream
# commit this branch is based on, the same as control/Dockerfile.

FROM ghcr.io/nvidia/openshell/supervisor:ea265a2f9a86e6fc1911041fec4b5fdcefd674dd AS supervisor
FROM ghcr.io/nvidia/openshell/sandbox:ea265a2f9a86e6fc1911041fec4b5fdcefd674dd AS sandbox

FROM ubuntu:24.04

# iproute2: the workload network namespace and its veth link. socat: the
# forwarder from the Tensorlake tunnel into that namespace. util-linux:
# unshare and setpriv. ca-certificates: guard TLS to upstreams and the
# tunnel's TLS to the Tensorlake proxy. curl: the tl installer.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl iproute2 socat util-linux \
    && rm -rf /var/lib/apt/lists/*

# Tensorlake CLI. The guard VM runs `tl sbx tunnel` to reach the workload
# VM's TLS listener. Pinned; the installer verifies the SHA256SUMS file.
ENV TENSORLAKE_VERSION=cli-v0.5.154
RUN curl -fsSL https://tensorlake.ai/install \
    | TENSORLAKE_INSTALL=/opt/openshell sh \
    && test -x /opt/openshell/bin/tl

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
