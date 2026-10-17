#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <signed-ipa-path>" >&2
    exit 64
fi

IPA_PATH="$1"

require_env() {
    VARIABLE_NAME="$1"
    eval "VARIABLE_VALUE=\${$VARIABLE_NAME:-}"
    if [ -z "$VARIABLE_VALUE" ]; then
        echo "missing required environment variable: $VARIABLE_NAME" >&2
        exit 65
    fi
}

decode_base64() {
    INPUT="$1"
    OUTPUT_PATH="$2"

    if printf '%s' "$INPUT" | base64 --decode >"$OUTPUT_PATH" 2>/dev/null; then
        return 0
    fi

    if printf '%s' "$INPUT" | base64 -D >"$OUTPUT_PATH" 2>/dev/null; then
        return 0
    fi

    echo "failed decoding App Store Connect private key" >&2
    exit 66
}

require_env "BERRYKEEP_APP_STORE_CONNECT_API_KEY_ID"
require_env "BERRYKEEP_APP_STORE_CONNECT_API_ISSUER_ID"
require_env "BERRYKEEP_APP_STORE_CONNECT_API_PRIVATE_KEY_B64"

if [ ! -r "$IPA_PATH" ]; then
    echo "signed IPA does not exist or is not readable: $IPA_PATH" >&2
    exit 67
fi

case "$BERRYKEEP_APP_STORE_CONNECT_API_KEY_ID" in
    *[!A-Za-z0-9]* | '')
        echo "App Store Connect API key ID must be alphanumeric" >&2
        exit 68
        ;;
esac

umask 077
PRIVATE_KEYS_DIR="$HOME/.appstoreconnect/private_keys"
PRIVATE_KEY_PATH="$PRIVATE_KEYS_DIR/AuthKey_${BERRYKEEP_APP_STORE_CONNECT_API_KEY_ID}.p8"

if [ -e "$PRIVATE_KEY_PATH" ]; then
    echo "refusing to overwrite existing App Store Connect private key: $PRIVATE_KEY_PATH" >&2
    exit 69
fi

cleanup() {
    rm -f "$PRIVATE_KEY_PATH"
    rmdir "$PRIVATE_KEYS_DIR" 2>/dev/null || true
    rmdir "$(dirname "$PRIVATE_KEYS_DIR")" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

mkdir -p "$PRIVATE_KEYS_DIR"
decode_base64 "$BERRYKEEP_APP_STORE_CONNECT_API_PRIVATE_KEY_B64" "$PRIVATE_KEY_PATH"

xcrun altool --upload-app \
    --file "$IPA_PATH" \
    --type ios \
    --apiKey "$BERRYKEEP_APP_STORE_CONNECT_API_KEY_ID" \
    --apiIssuer "$BERRYKEEP_APP_STORE_CONNECT_API_ISSUER_ID" \
    --output-format json
