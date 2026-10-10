#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

npx napi build --platform --js index.cjs --config-path napi.config.json

rm -rf susee_bundler.linux-x64-gnu.node