# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

# Control-plane image: the OpenShell gateway and the Tensorlake compute
# driver in one Tensorlake sandbox.
#
# The gateway binary comes from the published gateway image. The driver is
# built from the source tree that build-images.sh copies into `src/` next to
# this file. Do not build this file directly; run ../build-images.sh.
#
# Tensorlake builds use this directory as the build context, and do not
# expand variables in FROM, so the image tags are fixed here. Pin the
# upstream commit this branch is based on, the same as image/Dockerfile.

FROM ghcr.io/nvidia/openshell/gateway:a4c848adbddd0b4732fd70ddcf8a563a5b1e9317 AS gateway

FROM rust:1.95-trixie AS driver
WORKDIR /src
COPY src/ ./
RUN cargo build --release --locked -p openshell-driver-tensorlake \
    && install -m 0555 target/release/openshell-driver-tensorlake /openshell-driver-tensorlake

FROM debian:trixie-slim

# ca-certificates: the driver calls the Tensorlake API, and guards use the
# same bundle to trust the public certificate of the Tensorlake port proxy.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# The gateway and the driver must run as the same user.
RUN groupadd --gid 1000 openshell \
    && useradd --uid 1000 --gid 1000 --home-dir /var/lib/openshell --shell /usr/sbin/nologin openshell \
    && install -d -m 0700 -o openshell -g openshell /var/lib/openshell /etc/openshell-control

COPY --from=gateway /usr/local/bin/openshell-gateway /opt/openshell/bin/openshell-gateway
COPY --from=driver /openshell-driver-tensorlake /opt/openshell/bin/openshell-driver-tensorlake
COPY openshell-control-start.sh /opt/openshell/bin/openshell-control-start
# Written by build-images.sh: the paired sandbox image and the source commit.
COPY share/ /opt/openshell/share/
RUN chmod 0555 /opt/openshell/bin/openshell-gateway /opt/openshell/bin/openshell-control-start
