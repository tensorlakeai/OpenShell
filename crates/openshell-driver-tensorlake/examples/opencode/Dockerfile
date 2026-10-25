# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# OpenCode agent image for the Tensorlake driver case study.
#
# It starts from the OpenShell Tensorlake sandbox image, which holds the
# launcher and the workload runtime, and adds the OpenCode binary at the
# path named in providers/openrouter.yaml. Build it on Tensorlake:
#
#   tl sbx image create crates/openshell-driver-tensorlake/examples/opencode/Dockerfile \
#     --registered-name openshell-tensorlake-opencode --cas
#
# The binary comes from the GitHub release, not from the
# ghcr.io/anomalyco/opencode image. That image is Alpine, and its binary
# needs the musl loader, which the Ubuntu base does not have.
#
# Tensorlake does not expand variables in FROM, so the base image name is
# fixed here. It is the default sandbox image name. Change it to match your
# image pair, for example openshell-tensorlake-test for a dev build.

FROM openshell-tensorlake

ARG OPENCODE_VERSION=1.19.3
RUN curl -fsSL "https://github.com/anomalyco/opencode/releases/download/v${OPENCODE_VERSION}/opencode-linux-x64.tar.gz" \
    | tar -xz -C /usr/local/bin opencode \
    && chmod 0755 /usr/local/bin/opencode \
    && /usr/local/bin/opencode --version
