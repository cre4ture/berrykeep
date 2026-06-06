#!/usr/bin/env bash
set -euo pipefail

PACKAGE_ROOT="/usr/lib/ironmesh-client"
PACKAGED_BINARY="${PACKAGE_ROOT}/ironmesh"
PACKAGED_BACKUP="${PACKAGE_ROOT}/ironmesh.packaged-deb"

usage() {
  cat <<'EOF'
Usage: scripts/restore-packaged-ironmesh.sh

Restore the packaged `ironmesh` client binary after testing a local build.
EOF
}

log() {
  printf '[restore-packaged-ironmesh] %s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  local cmd="$1"
  command -v "$cmd" >/dev/null 2>&1 || fail "required command not found: $cmd"
}

main() {
  if [[ $# -gt 0 ]]; then
    case "$1" in
      --help|-h)
        usage
        exit 0
        ;;
      *)
        fail "unknown argument: $1"
        ;;
    esac
  fi

  require_command sudo
  require_command readlink

  sudo test -f "$PACKAGED_BACKUP" || fail "packaged backup not found: ${PACKAGED_BACKUP}"

  if sudo test -L "$PACKAGED_BINARY"; then
    log "removing local override symlink"
    sudo rm "$PACKAGED_BINARY"
  elif sudo test -e "$PACKAGED_BINARY"; then
    fail "${PACKAGED_BINARY} exists and is not a symlink; refusing to overwrite it"
  fi

  log "restoring packaged binary from ${PACKAGED_BACKUP}"
  sudo mv "$PACKAGED_BACKUP" "$PACKAGED_BINARY"
  log "active ironmesh target: $(readlink -f "$PACKAGED_BINARY")"
}

main "$@"
