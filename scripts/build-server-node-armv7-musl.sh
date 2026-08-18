#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Compatibility wrapper for the existing LuckFox/Cortex-A7 deployment path.
# Portable generic server-node artifacts use build-static-server-node.sh
# directly with an explicit target and variant ID.
exec "${ROOT_DIR}/scripts/build-static-server-node.sh" \
  --target armv7-unknown-linux-musleabihf \
  --target-cpu cortex-a7 \
  --variant-id armv7-cortex-a7 \
  "$@"
