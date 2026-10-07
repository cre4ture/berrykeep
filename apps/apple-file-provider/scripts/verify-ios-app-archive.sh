#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <archive-path>" >&2
    exit 64
fi

ARCHIVE_PATH="$1"
APP_PATH="$ARCHIVE_PATH/Products/Applications/BerryKeepIosApp.app"
APP_INFO_PLIST="$APP_PATH/Info.plist"
EXTENSION_PATH="$APP_PATH/PlugIns/BerryKeepIosFileProviderExtension.appex"
EXTENSION_INFO_PLIST="$EXTENSION_PATH/Info.plist"
EXPECTED_EXTENSION_BUNDLE_ID="dev.berrykeep.apple.iosapp.fileprovider"
PLIST_BUDDY="/usr/libexec/PlistBuddy"

fail() {
    echo "iOS archive verification failed: $1" >&2
    exit 1
}

[ -d "$ARCHIVE_PATH" ] || fail "archive not found at $ARCHIVE_PATH"
[ -d "$APP_PATH" ] || fail "app bundle not found at $APP_PATH"
[ -f "$APP_INFO_PLIST" ] || fail "app Info.plist not found at $APP_INFO_PLIST"
[ -d "$EXTENSION_PATH" ] || fail "File Provider extension not embedded at $EXTENSION_PATH"
[ -f "$EXTENSION_INFO_PLIST" ] || fail "extension Info.plist not found at $EXTENSION_INFO_PLIST"
[ -x "$PLIST_BUDDY" ] || fail "PlistBuddy not found at $PLIST_BUDDY"

EXTENSION_BUNDLE_ID="$($PLIST_BUDDY -c 'Print :CFBundleIdentifier' "$EXTENSION_INFO_PLIST")"
[ "$EXTENSION_BUNDLE_ID" = "$EXPECTED_EXTENSION_BUNDLE_ID" ] ||
    fail "unexpected extension bundle identifier: $EXTENSION_BUNDLE_ID"

EXTENSION_EXECUTABLE="$($PLIST_BUDDY -c 'Print :CFBundleExecutable' "$EXTENSION_INFO_PLIST")"
[ -x "$EXTENSION_PATH/$EXTENSION_EXECUTABLE" ] ||
    fail "extension executable not found at $EXTENSION_PATH/$EXTENSION_EXECUTABLE"

APP_MARKETING_VERSION="$($PLIST_BUDDY -c 'Print :CFBundleShortVersionString' "$APP_INFO_PLIST")"
EXTENSION_MARKETING_VERSION="$($PLIST_BUDDY -c 'Print :CFBundleShortVersionString' "$EXTENSION_INFO_PLIST")"
APP_BUILD_NUMBER="$($PLIST_BUDDY -c 'Print :CFBundleVersion' "$APP_INFO_PLIST")"
EXTENSION_BUILD_NUMBER="$($PLIST_BUDDY -c 'Print :CFBundleVersion' "$EXTENSION_INFO_PLIST")"

[ -n "$APP_MARKETING_VERSION" ] || fail "app marketing version is empty"
[ "$APP_MARKETING_VERSION" = "$EXTENSION_MARKETING_VERSION" ] ||
    fail "app and extension marketing versions differ: $APP_MARKETING_VERSION vs $EXTENSION_MARKETING_VERSION"

case "$APP_BUILD_NUMBER" in
    '' | *[!0-9]*) fail "app build number must be a positive integer: $APP_BUILD_NUMBER" ;;
esac
[ "$APP_BUILD_NUMBER" -gt 0 ] || fail "app build number must be positive: $APP_BUILD_NUMBER"
[ "$APP_BUILD_NUMBER" = "$EXTENSION_BUILD_NUMBER" ] ||
    fail "app and extension build numbers differ: $APP_BUILD_NUMBER vs $EXTENSION_BUILD_NUMBER"

if [ -n "${BERRYKEEP_IOS_EXPECTED_BUILD_NUMBER:-}" ]; then
    [ "$APP_BUILD_NUMBER" = "$BERRYKEEP_IOS_EXPECTED_BUILD_NUMBER" ] ||
        fail "unexpected app build number: expected $BERRYKEEP_IOS_EXPECTED_BUILD_NUMBER, got $APP_BUILD_NUMBER"
fi

printf 'Verified embedded File Provider extension: %s (version %s, build %s)\n' \
    "$EXTENSION_PATH" "$APP_MARKETING_VERSION" "$APP_BUILD_NUMBER"
