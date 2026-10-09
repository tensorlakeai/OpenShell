# syntax=docker/dockerfile:1.7

# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install --yes --no-install-recommends \
        ca-certificates \
        nodejs \
        npm \
    && rm -rf /var/lib/apt/lists/*

RUN useradd \
    --create-home \
    --home-dir /sandbox \
    --shell /bin/bash \
    sandbox

RUN npm install --global --ignore-scripts pnpm@11.28.5 && mkdir -p /usr/local/lib/tensorlake-tools && (test -f /usr/local/lib/tensorlake-tools/package.json || printf '%s\n' '{"private":true,"packageManager":"pnpm@11.28.5"}' > /usr/local/lib/tensorlake-tools/package.json) && pnpm --dir /usr/local/lib/tensorlake-tools --config.minimumReleaseAge=1440 --config.minimumReleaseAgeStrict=true --config.minimumReleaseAgeIgnoreMissingTime=false --config.trustLockfile=false --config.blockExoticSubdeps=true --config.strictDepBuilds=true --config.optimisticRepeatInstall=false add --ignore-scripts --save-exact @openai/codex@0.161.0 && for executable in /usr/local/lib/tensorlake-tools/node_modules/.bin/*; do [ ! -e "$executable" ] || ln -sf "$executable" /usr/local/bin/; done \
    && npm cache clean --force \
    && codex --version

COPY --chmod=0755 start-codex-app-server /usr/local/bin/start-codex-app-server

ENV CODEX_APP_SERVER_PORT=4500
ENV HOME=/sandbox

USER sandbox
WORKDIR /sandbox
