#!/bin/sh
set -eu

usage() {
    cat <<'EOF'
usage: scripts/run-ios-simulator-app.sh [--reset]

Builds the current iOS app, installs it into the resolved simulator device,
and launches it.

Options:
  --reset   Erase and reboot the target simulator before install
  --help    Show this help text

Supported environment overrides are forwarded to the underlying launcher,
including:
  IRONMESH_IOS_PROJECT_PATH
  IRONMESH_IOS_APP_SCHEME
  IRONMESH_IOS_APP_BUNDLE_ID
  IRONMESH_IOS_BUILD_CONFIGURATION
  IRONMESH_IOS_DERIVED_DATA_PATH
EOF
}

RESET=0

case "${1:-}" in
    "")
        ;;
    --reset)
        RESET=1
        shift
        ;;
    --help|-h)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 64
        ;;
esac

if [ "$#" -ne 0 ]; then
    usage >&2
    exit 64
fi

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)

IRONMESH_IOS_SIMULATOR_RESET="$RESET" \
    exec "$REPO_ROOT/apps/apple-file-provider/scripts/run-ios-simulator-app.sh"
