#!/bin/sh
set -eu

: "${IOS_PROJECT_PATH:?missing IOS_PROJECT_PATH}"
: "${IOS_APP_SCHEME:?missing IOS_APP_SCHEME}"
: "${IOS_DERIVED_DATA_PATH:?missing IOS_DERIVED_DATA_PATH}"
: "${IOS_ARCHIVE_PATH:?missing IOS_ARCHIVE_PATH}"
: "${IOS_BUILD_NUMBER:?missing IOS_BUILD_NUMBER}"

if [ "${IOS_SIGNED_EXPORT_ENABLED:-false}" = "true" ]; then
    : "${BERRYKEEP_IOS_SIGNING_IDENTITY:?missing BERRYKEEP_IOS_SIGNING_IDENTITY}"
    : "${BERRYKEEP_IOS_DEVELOPMENT_TEAM:?missing BERRYKEEP_IOS_DEVELOPMENT_TEAM}"
    : "${BERRYKEEP_IOS_KEYCHAIN_PATH:?missing BERRYKEEP_IOS_KEYCHAIN_PATH}"
    : "${BERRYKEEP_IOS_APP_PROFILE_UUID:?missing BERRYKEEP_IOS_APP_PROFILE_UUID}"
    : "${BERRYKEEP_IOS_EXTENSION_PROFILE_UUID:?missing BERRYKEEP_IOS_EXTENSION_PROFILE_UUID}"

    # Expand the profile by product in Xcode, so the host and extension each
    # receive their own profile and Swift package products receive none.
    set -- \
        CODE_SIGN_STYLE=Manual \
        "CODE_SIGN_IDENTITY=$BERRYKEEP_IOS_SIGNING_IDENTITY" \
        CODE_SIGNING_ALLOWED=YES \
        CODE_SIGNING_REQUIRED=YES \
        "DEVELOPMENT_TEAM=$BERRYKEEP_IOS_DEVELOPMENT_TEAM" \
        'PROVISIONING_PROFILE_SPECIFIER=$(BERRYKEEP_IOS_PROFILE_$(PRODUCT_NAME))' \
        "BERRYKEEP_IOS_PROFILE_BerryKeepIosApp=$BERRYKEEP_IOS_APP_PROFILE_UUID" \
        "BERRYKEEP_IOS_PROFILE_BerryKeepIosFileProviderExtension=$BERRYKEEP_IOS_EXTENSION_PROFILE_UUID" \
        "OTHER_CODE_SIGN_FLAGS=--keychain $BERRYKEEP_IOS_KEYCHAIN_PATH" \
        "$@"
else
    set -- CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO "$@"
fi

exec xcodebuild archive \
    -project "$IOS_PROJECT_PATH" \
    -scheme "$IOS_APP_SCHEME" \
    -configuration Release \
    -destination "generic/platform=iOS" \
    -derivedDataPath "$IOS_DERIVED_DATA_PATH" \
    -archivePath "$IOS_ARCHIVE_PATH" \
    "CURRENT_PROJECT_VERSION=$IOS_BUILD_NUMBER" \
    "$@"
