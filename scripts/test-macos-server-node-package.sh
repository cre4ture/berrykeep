#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILDER="${ROOT_DIR}/scripts/build-macos-server-node-pkg.sh"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/berrykeep-server-node-package-test.XXXXXX")"

cleanup() {
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

[[ "$(uname -s)" == "Darwin" ]] || fail "this test must run on macOS"
require_command pkgutil
require_command plutil

bash -n "${BUILDER}"
bash -n "${ROOT_DIR}/scripts/uninstall-macos-server-node.sh"
bash -n "${ROOT_DIR}/macos/server-node/berrykeep-server-node-launcher"
bash -n "${ROOT_DIR}/macos/server-node/pkg-scripts/preinstall"
bash -n "${ROOT_DIR}/macos/server-node/pkg-scripts/postinstall"
plutil -lint "${ROOT_DIR}/macos/server-node/io.berrykeep.server-node.plist" >/dev/null

fixture_binary="${TMP_DIR}/berrykeep-server-node"
printf '#!/bin/sh\nexit 0\n' > "${fixture_binary}"
chmod 0755 "${fixture_binary}"

package_path="${TMP_DIR}/berrykeep-server-node.pkg"
"${BUILDER}" --binary "${fixture_binary}" --output "${package_path}"

payload_files="$(pkgutil --payload-files "${package_path}")"
for expected_path in \
  './Library/Application Support/BerryKeep/bin/berrykeep-server-node' \
  './Library/Application Support/BerryKeep/bin/berrykeep-server-node-launcher' \
  './Library/Application Support/BerryKeep/server-node.env.example' \
  './Library/LaunchDaemons/io.berrykeep.server-node.plist'; do
  printf '%s\n' "${payload_files}" | grep -Fqx "${expected_path}" \
    || fail "package payload is missing ${expected_path}"
done

# The package contains only canonical executables. A macOS server-node install
# is a fresh setup, so it must not include an additional server binary.
unexpected_binary_paths="$(printf '%s\n' "${payload_files}" | awk '
  /^\.\/Library\/Application Support\/BerryKeep\/bin\// &&
  $0 != "./Library/Application Support/BerryKeep/bin/berrykeep-server-node" &&
  $0 != "./Library/Application Support/BerryKeep/bin/berrykeep-server-node-launcher" { print }
')"
if [[ -n "${unexpected_binary_paths}" ]]; then
  fail "package payload contains unexpected server binaries: ${unexpected_binary_paths}"
fi

# Mutable directories are created by post-install and are not part of the
# package payload.
for unexpected_path in \
  './Library/Application Support/BerryKeep/server-node' \
  './Library/Logs/BerryKeep'; do
  if printf '%s\n' "${payload_files}" | grep -Fqx "${unexpected_path}"; then
    fail "package payload must not pre-create mutable directory ${unexpected_path}"
  fi
done

printf 'macOS server-node package structure is valid\n'
