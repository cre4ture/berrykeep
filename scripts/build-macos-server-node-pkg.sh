#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_ID="io.ironmesh.server-node"
PACKAGE_VERSION="$(awk -F '"' '/^version = "/ { print $2; exit }' "${ROOT_DIR}/Cargo.toml")"
PACKAGE_VERSION="${PACKAGE_VERSION:?failed to determine workspace package version}"
CARGO_BUILD_DIR=""

OUTPUT_PATH="${ROOT_DIR}/target/macos/ironmesh-server-node-${PACKAGE_VERSION}.pkg"
SOURCE_BINARY=""
ARCH="native"
CODE_SIGN_IDENTITY=""
INSTALLER_SIGN_IDENTITY=""
STAGE_DIR=""

log() {
  printf '[build-macos-server-node-pkg] %s\n' "$*"
}

fail() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

usage() {
  cat <<'EOF'
Build a native macOS installer package for the Ironmesh server node.

Usage:
  ./scripts/build-macos-server-node-pkg.sh [options]

Options:
  --output PATH                    Write the package to PATH.
  --binary PATH                    Package an already-built server-node binary.
  --arch ARCH                      Build native, arm64, x86_64, or universal
                                   (default: native).
  --code-sign-identity IDENTITY    Sign the staged executable with a Developer
                                   ID Application identity.
  --installer-sign-identity IDENTITY
                                   Sign the resulting package with a Developer
                                   ID Installer identity.
  -h, --help                       Show this help.

The default build compiles the binary for the current Mac. `--arch universal`
requires both aarch64-apple-darwin and x86_64-apple-darwin Rust targets plus
the Xcode `lipo` tool. Supplying --binary skips Cargo entirely, which is useful
for packaging a release artifact or validating package structure in CI.
EOF
}

while (($# > 0)); do
  case "$1" in
    --output)
      (($# >= 2)) || fail "--output requires a value"
      OUTPUT_PATH="$2"
      shift 2
      ;;
    --output=*)
      OUTPUT_PATH="${1#*=}"
      shift
      ;;
    --binary)
      (($# >= 2)) || fail "--binary requires a value"
      SOURCE_BINARY="$2"
      shift 2
      ;;
    --binary=*)
      SOURCE_BINARY="${1#*=}"
      shift
      ;;
    --arch)
      (($# >= 2)) || fail "--arch requires a value"
      ARCH="$2"
      shift 2
      ;;
    --arch=*)
      ARCH="${1#*=}"
      shift
      ;;
    --code-sign-identity)
      (($# >= 2)) || fail "--code-sign-identity requires a value"
      CODE_SIGN_IDENTITY="$2"
      shift 2
      ;;
    --code-sign-identity=*)
      CODE_SIGN_IDENTITY="${1#*=}"
      shift
      ;;
    --installer-sign-identity)
      (($# >= 2)) || fail "--installer-sign-identity requires a value"
      INSTALLER_SIGN_IDENTITY="$2"
      shift 2
      ;;
    --installer-sign-identity=*)
      INSTALLER_SIGN_IDENTITY="${1#*=}"
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

build_target() {
  local target="$1"

  log "building ironmesh-server-node for ${target}"
  (
    cd "${ROOT_DIR}"
    cargo build --locked --release -p server-node --bin ironmesh-server-node --target "${target}"
  )
}

resolve_cargo_build_dir() {
  local target_directory

  target_directory="$(
    cd "${ROOT_DIR}"
    cargo metadata --locked --no-deps --format-version 1 \
      | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p'
  )"
  [[ -n "${target_directory}" ]] \
    || fail "could not determine Cargo's target directory"
  printf '%s\n' "${target_directory}"
}

build_source_binary() {
  local universal_dir

  if [[ -n "${SOURCE_BINARY}" ]]; then
    [[ -f "${SOURCE_BINARY}" ]] || fail "server-node binary was not found: ${SOURCE_BINARY}"
    printf '%s\n' "${SOURCE_BINARY}"
    return
  fi

  CARGO_BUILD_DIR="$(resolve_cargo_build_dir)"

  case "${ARCH}" in
    native)
      log "building ironmesh-server-node for the current Mac" >&2
      (
        cd "${ROOT_DIR}"
        cargo build --locked --release -p server-node --bin ironmesh-server-node
      )
      printf '%s\n' "${CARGO_BUILD_DIR}/release/ironmesh-server-node"
      ;;
    arm64)
      build_target aarch64-apple-darwin >&2
      printf '%s\n' "${CARGO_BUILD_DIR}/aarch64-apple-darwin/release/ironmesh-server-node"
      ;;
    x86_64)
      build_target x86_64-apple-darwin >&2
      printf '%s\n' "${CARGO_BUILD_DIR}/x86_64-apple-darwin/release/ironmesh-server-node"
      ;;
    universal)
      require_command lipo
      build_target aarch64-apple-darwin >&2
      build_target x86_64-apple-darwin >&2
      universal_dir="${CARGO_BUILD_DIR}/macos/universal"
      mkdir -p "${universal_dir}"
      lipo -create -output "${universal_dir}/ironmesh-server-node" \
        "${CARGO_BUILD_DIR}/aarch64-apple-darwin/release/ironmesh-server-node" \
        "${CARGO_BUILD_DIR}/x86_64-apple-darwin/release/ironmesh-server-node"
      printf '%s\n' "${universal_dir}/ironmesh-server-node"
      ;;
    *)
      fail "--arch must be native, arm64, x86_64, or universal"
      ;;
  esac
}

cleanup() {
  if [[ -n "${STAGE_DIR}" ]]; then
    rm -rf "${STAGE_DIR}"
  fi
}

main() {
  local payload_dir scripts_dir binary_path pkgbuild_args

  [[ "$(uname -s)" == "Darwin" ]] || fail "this packaging helper must run on macOS"
  require_command pkgbuild
  require_command install
  require_command xattr
  if [[ -z "${SOURCE_BINARY}" ]]; then
    require_command cargo
  fi

  binary_path="$(build_source_binary)"
  [[ -f "${binary_path}" ]] || fail "expected built server-node binary was not found: ${binary_path}"

  STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ironmesh-server-node-pkg.XXXXXX")"
  trap cleanup EXIT
  payload_dir="${STAGE_DIR}/payload"
  scripts_dir="${ROOT_DIR}/macos/server-node/pkg-scripts"

  install -d "${payload_dir}/Library/Application Support/Ironmesh/bin"
  install -d "${payload_dir}/Library/Application Support/Ironmesh/server-node"
  install -d "${payload_dir}/Library/LaunchDaemons"
  install -d "${payload_dir}/Library/Logs/Ironmesh"
  install -m 0755 "${binary_path}" \
    "${payload_dir}/Library/Application Support/Ironmesh/bin/ironmesh-server-node"
  install -m 0755 "${ROOT_DIR}/macos/server-node/ironmesh-server-node-launcher" \
    "${payload_dir}/Library/Application Support/Ironmesh/bin/ironmesh-server-node-launcher"
  install -m 0644 "${ROOT_DIR}/macos/server-node/server-node.env.example" \
    "${payload_dir}/Library/Application Support/Ironmesh/server-node.env.example"
  install -m 0644 "${ROOT_DIR}/macos/server-node/io.ironmesh.server-node.plist" \
    "${payload_dir}/Library/LaunchDaemons/${PACKAGE_ID}.plist"

  if [[ -n "${CODE_SIGN_IDENTITY}" ]]; then
    require_command codesign
    log "signing staged server-node executable"
    codesign --force --options runtime --timestamp --sign "${CODE_SIGN_IDENTITY}" \
      "${payload_dir}/Library/Application Support/Ironmesh/bin/ironmesh-server-node"
  fi

  # Do not carry Finder or provenance attributes from a checkout or build
  # volume into a release payload.
  xattr -cr "${payload_dir}"

  mkdir -p "$(dirname "${OUTPUT_PATH}")"
  pkgbuild_args=(
    --root "${payload_dir}"
    --scripts "${scripts_dir}"
    --identifier "${PACKAGE_ID}"
    --version "${PACKAGE_VERSION}"
    --install-location /
    --ownership recommended
  )
  if [[ -n "${INSTALLER_SIGN_IDENTITY}" ]]; then
    pkgbuild_args+=(--sign "${INSTALLER_SIGN_IDENTITY}")
  fi

  COPYFILE_DISABLE=1 pkgbuild "${pkgbuild_args[@]}" "${OUTPUT_PATH}"
  log "built package: ${OUTPUT_PATH}"
}

main
