#!/usr/bin/env bash

# SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0

set -euo pipefail

version="${1:-${CODEX_VERSION:-latest}}"

if [[ "$version" != "latest" && ! "$version" =~ ^[0-9]+(\.[0-9]+){0,2}(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "unsupported Codex version: $version" >&2
    exit 2
fi

npm install --global --ignore-scripts pnpm@11.28.5 && mkdir -p "$(npm prefix --global)"/lib/tensorlake-tools && (test -f "$(npm prefix --global)"/lib/tensorlake-tools/package.json || printf '%s\n' '{"private":true,"packageManager":"pnpm@11.28.5"}' > "$(npm prefix --global)"/lib/tensorlake-tools/package.json) && pnpm --dir "$(npm prefix --global)"/lib/tensorlake-tools --config.minimumReleaseAge=1440 --config.minimumReleaseAgeStrict=true --config.minimumReleaseAgeIgnoreMissingTime=false --config.trustLockfile=false --config.blockExoticSubdeps=true --config.strictDepBuilds=true --config.optimisticRepeatInstall=false add --ignore-scripts --save-exact "@openai/codex@${version}" && for executable in "$(npm prefix --global)"/lib/tensorlake-tools/node_modules/.bin/*; do [ ! -e "$executable" ] || ln -sf "$executable" "$(npm prefix --global)"/bin/; done
npm cache clean --force >/dev/null 2>&1 || true
codex --version
