#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACT_DIR="$(cd "${ROOT_DIR}/.." && pwd)"
RUN_PREPARE=true
RUN_LINTIAN=false
CHECK_BUILD_DEPENDENCIES=true
PREBUILT_BINARIES_DIR=""
DPKG_BUILD_ARGS=()

log() {
  printf '[build-local-debs] %s\n' "$*"
}

require_clean_repository() {
  local status_output

  status_output="$(git -C "${ROOT_DIR}" status --short --untracked-files=normal)"
  if [[ -z "${status_output}" ]]; then
    return 0
  fi

  printf 'local repository state is dirty; commit, stash, or clean these paths before building local Debian packages:\n' >&2
  printf '%s\n' "${status_output}" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Build installable local Debian binary packages from the current checkout.

Usage:
  ./scripts/build-local-debs.sh [options] [-- <dpkg-buildpackage args>]

Options:
  --no-prepare  Skip ./scripts/prepare-ppa-source.sh.
  --lintian     Run lintian on the generated .changes file after a successful build.
  --prebuilt-binaries DIR
                Package the seven release binaries already present in DIR
                instead of preparing sources and compiling them. This skips
                the build-dependency check and validates the binary bundle.
  --no-check-build-deps
                Skip dpkg-checkbuilddeps and pass -d to dpkg-buildpackage.
                This is for native builders that provide the required Rust
                toolchain outside the distribution package manager.
  -h, --help    Show this help text.

Notes:
  - This helper builds local binary packages with dpkg-buildpackage -b -us -uc.
  - It is separate from ./scripts/build-ppa-source.sh, which builds Launchpad/PPA source uploads.
EOF
}

require_command() {
  local command_name="$1"

  if command -v "${command_name}" >/dev/null 2>&1; then
    return 0
  fi

  printf '%s is required but was not found in PATH\n' "${command_name}" >&2
  exit 1
}

check_build_dependencies() {
  local output

  if output="$(cd "${ROOT_DIR}" && dpkg-checkbuilddeps 2>&1)"; then
    return 0
  fi

  printf '%s\n' "${output}" >&2
  printf '\n' >&2
  printf 'Install the Debian build dependencies from %s and rerun.\n' \
    "${ROOT_DIR}/debian/control" >&2
  printf 'If deb-src entries are enabled, you can usually run:\n' >&2
  printf '  cd %q && sudo apt build-dep .\n' "${ROOT_DIR}" >&2
  exit 1
}

validate_prebuilt_binaries() {
  local binary
  local -a expected_binaries=(
    ironmesh-server-node
    ironmesh-rendezvous-service
    ironmesh
    ironmesh-config-app
    ironmesh-background-launcher
    ironmesh-folder-agent
    ironmesh-os-integration
  )

  for binary in "${expected_binaries[@]}"; do
    if [[ ! -x "${PREBUILT_BINARIES_DIR}/${binary}" ]]; then
      printf 'expected executable prebuilt binary not found: %s\n' \
        "${PREBUILT_BINARIES_DIR}/${binary}" >&2
      exit 1
    fi
  done
}

while (($# > 0)); do
  case "$1" in
    --no-prepare)
      RUN_PREPARE=false
      shift
      ;;
    --lintian)
      RUN_LINTIAN=true
      shift
      ;;
    --no-check-build-deps)
      CHECK_BUILD_DEPENDENCIES=false
      shift
      ;;
    --prebuilt-binaries)
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--prebuilt-binaries requires a directory' >&2
        exit 1
      }
      PREBUILT_BINARIES_DIR="$2"
      RUN_PREPARE=false
      CHECK_BUILD_DEPENDENCIES=false
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      DPKG_BUILD_ARGS+=("$@")
      break
      ;;
    *)
      printf 'unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

require_command git
require_command dpkg-buildpackage
require_command dpkg-parsechangelog
require_command dpkg

if [[ "${CHECK_BUILD_DEPENDENCIES}" == true ]]; then
  require_command dpkg-checkbuilddeps
fi

if [[ "${RUN_LINTIAN}" == true ]]; then
  require_command lintian
fi

require_clean_repository
if [[ -n "${PREBUILT_BINARIES_DIR}" ]]; then
  [[ -d "${PREBUILT_BINARIES_DIR}" ]] || {
    printf 'prebuilt binary directory does not exist: %s\n' \
      "${PREBUILT_BINARIES_DIR}" >&2
    exit 1
  }
  PREBUILT_BINARIES_DIR="$(cd "${PREBUILT_BINARIES_DIR}" && pwd)"

  "${ROOT_DIR}/scripts/sync-debian-version.sh" --check
  validate_prebuilt_binaries
  export IRONMESH_PREBUILT_BIN_DIR="${PREBUILT_BINARIES_DIR}"
  export IRONMESH_USE_PREBUILT_BINARIES=1
  log "packaging prebuilt binaries from ${PREBUILT_BINARIES_DIR}"
  DPKG_BUILD_ARGS=(-d "${DPKG_BUILD_ARGS[@]}")
else
  "${ROOT_DIR}/scripts/sync-debian-version.sh"
  if [[ "${CHECK_BUILD_DEPENDENCIES}" == true ]]; then
    check_build_dependencies
  else
    log "skipping dpkg build-dependency check"
    DPKG_BUILD_ARGS=(-d "${DPKG_BUILD_ARGS[@]}")
  fi

  if [[ "${RUN_PREPARE}" == true ]]; then
    log "preparing vendored crates and prebuilt web assets"
    "${ROOT_DIR}/scripts/prepare-ppa-source.sh"
  fi
fi

SOURCE_NAME="$(cd "${ROOT_DIR}" && dpkg-parsechangelog -SSource)"
VERSION="$(cd "${ROOT_DIR}" && dpkg-parsechangelog -SVersion)"
ARCH="$(dpkg --print-architecture)"
CHANGES_PATH="${ARTIFACT_DIR}/${SOURCE_NAME}_${VERSION}_${ARCH}.changes"
BUILDINFO_PATH="${ARTIFACT_DIR}/${SOURCE_NAME}_${VERSION}_${ARCH}.buildinfo"
PACKAGE_PATHS=(
  "${ARTIFACT_DIR}/ironmesh-client_${VERSION}_${ARCH}.deb"
  "${ARTIFACT_DIR}/ironmesh-server-node_${VERSION}_${ARCH}.deb"
  "${ARTIFACT_DIR}/ironmesh-rendezvous-service_${VERSION}_${ARCH}.deb"
)

log "building local Debian binary packages"
(
  cd "${ROOT_DIR}"
  dpkg-buildpackage -b -us -uc "${DPKG_BUILD_ARGS[@]}"
)

for path in "${PACKAGE_PATHS[@]}" "${CHANGES_PATH}" "${BUILDINFO_PATH}"; do
  if [[ ! -f "${path}" ]]; then
    printf 'expected build artifact not found: %s\n' "${path}" >&2
    exit 1
  fi
done

log "built artifacts:"
for path in "${PACKAGE_PATHS[@]}" "${CHANGES_PATH}" "${BUILDINFO_PATH}"; do
  printf '  %s\n' "${path}"
done

printf '\n'
log "install locally with:"
printf '  sudo apt install'
for path in "${PACKAGE_PATHS[@]}"; do
  printf ' %q' "${path}"
done
printf '\n'

if [[ "${RUN_LINTIAN}" == true ]]; then
  printf '\n'
  log "running lintian on ${CHANGES_PATH}"
  lintian "${CHANGES_PATH}"
fi
