#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
UPLOAD_SCRIPT="${SCRIPT_DIR}/upload-ios-testflight-ipa.sh"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/berrykeep-testflight-upload.XXXXXX")"

cleanup() {
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT HUP INT TERM

FAKE_BIN="$TEST_ROOT/bin"
FAKE_HOME="$TEST_ROOT/home"
IPA_PATH="$TEST_ROOT/BerryKeep.ipa"
ARGS_PATH="$TEST_ROOT/altool-args"
KEY_CONTENT_PATH="$TEST_ROOT/key-content"
mkdir -p "$FAKE_BIN" "$FAKE_HOME"
printf '%s' 'not-a-real-ipa' >"$IPA_PATH"

cat >"$FAKE_BIN/xcrun" <<'EOF'
#!/bin/sh
set -eu

[ "$1" = "altool" ]
shift
printf '%s\n' "$@" >"$TEST_ARGS_PATH"
cat "$HOME/.appstoreconnect/private_keys/AuthKey_ABC123DEF4.p8" >"$TEST_KEY_CONTENT_PATH"
EOF
chmod +x "$FAKE_BIN/xcrun"

API_KEY_B64="$(printf '%s' 'test-private-key' | base64 | tr -d '\n')"
PATH="$FAKE_BIN:$PATH" \
HOME="$FAKE_HOME" \
TEST_ARGS_PATH="$ARGS_PATH" \
TEST_KEY_CONTENT_PATH="$KEY_CONTENT_PATH" \
BERRYKEEP_APP_STORE_CONNECT_API_KEY_ID="ABC123DEF4" \
BERRYKEEP_APP_STORE_CONNECT_API_ISSUER_ID="11111111-2222-3333-4444-555555555555" \
BERRYKEEP_APP_STORE_CONNECT_API_PRIVATE_KEY_B64="$API_KEY_B64" \
"$UPLOAD_SCRIPT" "$IPA_PATH"

EXPECTED_ARGS="--upload-app
--file
$IPA_PATH
--type
ios
--apiKey
ABC123DEF4
--apiIssuer
11111111-2222-3333-4444-555555555555
--output-format
json"

[ "$(cat "$ARGS_PATH")" = "$EXPECTED_ARGS" ]
[ "$(cat "$KEY_CONTENT_PATH")" = "test-private-key" ]
[ ! -e "$FAKE_HOME/.appstoreconnect/private_keys/AuthKey_ABC123DEF4.p8" ]

if PATH="$FAKE_BIN:$PATH" HOME="$FAKE_HOME" \
    BERRYKEEP_APP_STORE_CONNECT_API_KEY_ID="ABC123DEF4" \
    BERRYKEEP_APP_STORE_CONNECT_API_PRIVATE_KEY_B64="$API_KEY_B64" \
    "$UPLOAD_SCRIPT" "$IPA_PATH" >/dev/null 2>&1; then
    echo "upload unexpectedly accepted a missing issuer ID" >&2
    exit 1
fi
