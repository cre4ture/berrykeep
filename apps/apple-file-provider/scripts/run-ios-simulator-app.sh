#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
APP_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
REPO_ROOT=$(CDPATH= cd -- "$APP_DIR/../.." && pwd)
PROJECT_PATH_INPUT="${BERRYKEEP_IOS_PROJECT_PATH:-$APP_DIR/BerryKeepAppleFileProvider.xcodeproj}"
SCHEME="${BERRYKEEP_IOS_APP_SCHEME:-BerryKeepIosApp}"
BUNDLE_ID="${BERRYKEEP_IOS_APP_BUNDLE_ID:-dev.ironmesh.apple.iosapp}"
BUILD_CONFIGURATION="${BERRYKEEP_IOS_BUILD_CONFIGURATION:-Debug}"
DERIVED_DATA_PATH="${BERRYKEEP_IOS_DERIVED_DATA_PATH:-/tmp/berrykeep-ios-app-sim-derived-data}"

case "$PROJECT_PATH_INPUT" in
    /*) PROJECT_PATH="$PROJECT_PATH_INPUT" ;;
    *) PROJECT_PATH="$REPO_ROOT/$PROJECT_PATH_INPUT" ;;
esac

DESTINATION="$(
    BERRYKEEP_IOS_SIMULATOR_APP_BUNDLE_ID="$BUNDLE_ID" \
        "$SCRIPT_DIR/prepare-ios-simulator.sh" "$PROJECT_PATH" "$SCHEME"
)"
UDID="${DESTINATION##*=}"
APP_PATH="${DERIVED_DATA_PATH}/Build/Products/${BUILD_CONFIGURATION}-iphonesimulator/BerryKeepIosApp.app"

rm -rf "$DERIVED_DATA_PATH"

xcodebuild build \
    -project "$PROJECT_PATH" \
    -scheme "$SCHEME" \
    -configuration "$BUILD_CONFIGURATION" \
    -destination "$DESTINATION" \
    -destination-timeout 180 \
    -derivedDataPath "$DERIVED_DATA_PATH"

xcrun simctl install "$UDID" "$APP_PATH"
open -a Simulator
xcrun simctl launch "$UDID" "$BUNDLE_ID"

printf 'Installed and launched %s on %s\n' "$BUNDLE_ID" "$DESTINATION"
