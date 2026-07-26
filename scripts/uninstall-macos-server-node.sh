#!/usr/bin/env bash

set -euo pipefail

readonly LABEL="io.ironmesh.server-node"
readonly PLIST_PATH="/Library/LaunchDaemons/${LABEL}.plist"
readonly SERVICE_ROOT="/Library/Application Support/Ironmesh"
readonly LOG_DIR="/Library/Logs/Ironmesh"

PURGE_DATA=false

usage() {
  cat <<'EOF'
Uninstall the packaged Ironmesh macOS server node.

Usage:
  sudo ./scripts/uninstall-macos-server-node.sh [--purge-data]

The default removal stops and unregisters the LaunchDaemon, then removes the
installed binary, launcher, LaunchDaemon plist, and logs. It deliberately
preserves server-node.env and the server-node data directory. Pass
--purge-data only when those configuration, identities, and stored files should
also be permanently removed.
EOF
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

while (($# > 0)); do
  case "$1" in
    --purge-data)
      PURGE_DATA=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1"
      ;;
  esac
done

[[ "$(id -u)" == "0" ]] || fail "run this script with sudo"

launchctl bootout "system/${LABEL}" >/dev/null 2>&1 || true
launchctl bootout system "${PLIST_PATH}" >/dev/null 2>&1 || true
rm -f "${PLIST_PATH}"
rm -f "${SERVICE_ROOT}/bin/ironmesh-server-node" \
  "${SERVICE_ROOT}/bin/ironmesh-server-node-launcher" \
  "${SERVICE_ROOT}/server-node.env.example"
rmdir "${SERVICE_ROOT}/bin" 2>/dev/null || true
rm -f "${LOG_DIR}/server-node.stdout.log" "${LOG_DIR}/server-node.stderr.log"
rmdir "${LOG_DIR}" 2>/dev/null || true

if [[ "${PURGE_DATA}" == true ]]; then
  rm -rf "${SERVICE_ROOT}/server-node" "${SERVICE_ROOT}/server-node.env"
fi

rmdir "${SERVICE_ROOT}" 2>/dev/null || true
pkgutil --forget "${LABEL}" >/dev/null 2>&1 || true

printf 'Ironmesh server node uninstalled'
if [[ "${PURGE_DATA}" == false ]]; then
  printf '; configuration and server data were preserved'
fi
printf '.\n'
