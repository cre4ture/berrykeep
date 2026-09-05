#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACT_DIR="$(cd "${ROOT_DIR}/.." && pwd)"
RUN_PREPARE=true
RUN_LINTIAN=false
CHECK_BUILD_DEPENDENCIES=true
PREBUILT_BINARIES_DIR=""
PREBUILT_SERVER_NODE=""
STATIC_SERVER_NODE_ARTIFACT=""
STATIC_ARTIFACT_TEMP_DIR=""
SERVER_NODE_ONLY=false
TARGET_SUITE="${APT_REPO_SUITE:-}"
TARGET_ARCH=""
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
  --suite SUITE  Build packages on a native builder for this apt suite.
                 Defaults to APT_REPO_SUITE or the local VERSION_CODENAME.
  --no-prepare  Skip ./scripts/prepare-ppa-source.sh.
  --lintian     Run lintian on the generated .changes file after a successful build.
  --prebuilt-binaries DIR
                Package the seven release binaries already present in DIR
                instead of preparing sources and compiling them. This skips
                the build-dependency check and validates the binary bundle.
  --prebuilt-server-node FILE
                Package this executable as ironmesh-server-node while building
                the remaining binaries from source. This supports reusing the
                verified static musl server artifact across distribution builds.
  --server-node-only
                Build only the portable ironmesh-server-node package. This
                requires --prebuilt-server-node or --static-server-node-artifact
                and skips client, rendezvous, map-tools, source preparation,
                and Rust compilation.
  --arch ARCH   Build a server-node-only package for this Debian architecture.
                Defaults to the native architecture. Cross-architecture
                package assembly is supported only with --server-node-only.
  --static-server-node-artifact ARCHIVE
                Verify and extract a .tar.gz emitted by
                build-static-server-node.sh, then package its Server Node.
                The adjacent .sha256 file is required. The artifact must name
                this clean checkout's Git revision and target architecture.
  --no-check-build-deps
                Skip dpkg-checkbuilddeps and pass -d to dpkg-buildpackage.
                This is for native builders that provide the required Rust
                toolchain outside the distribution package manager.
  -h, --help    Show this help text.

Notes:
  - This helper builds local binary packages with dpkg-buildpackage -b -us -uc.
  - It is separate from ./scripts/build-ppa-source.sh, which builds Launchpad/PPA source uploads.
  - A package suite must match the native builder distribution. Static musl
    binaries do not eliminate Debian helper-package dependencies in the .deb.
  - Server-node-only packages can be assembled for another Debian architecture
    because the profile neither executes nor compiles the static server binary.
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

native_suite() {
  local os_id os_suite

  if [[ ! -r /etc/os-release ]]; then
    printf '%s\n' 'unable to identify the native build suite: /etc/os-release is not readable' >&2
    exit 1
  fi

  # shellcheck disable=SC1091
  . /etc/os-release
  os_id="${ID:-unknown}"
  os_suite="${VERSION_CODENAME:-}"

  if [[ -z "${os_suite}" ]]; then
    printf 'unable to identify the native build suite for %s: VERSION_CODENAME is empty\n' \
      "${os_id}" >&2
    exit 1
  fi

  printf '%s\n' "${os_suite}"
}

validate_suite_name() {
  local suite="$1"

  if [[ ! "${suite}" =~ ^[a-z0-9][a-z0-9.-]*$ ]]; then
    printf 'invalid apt suite: %s\n' "${suite}" >&2
    exit 1
  fi
}

validate_architecture() {
  local architecture="$1"

  if [[ ! "${architecture}" =~ ^[a-z0-9][a-z0-9-]*$ ]]; then
    printf 'invalid Debian architecture: %s\n' "${architecture}" >&2
    exit 1
  fi
}

expected_elf_machine() {
  local architecture="$1"

  case "${architecture}" in
    amd64) printf '%s\n' 'Advanced Micro Devices X86-64' ;;
    arm64) printf '%s\n' 'AArch64' ;;
    armhf) printf '%s\n' 'ARM' ;;
    *)
      printf 'unsupported server-node-only Debian architecture: %s\n' \
        "${architecture}" >&2
      exit 1
      ;;
  esac
}

expected_rust_target() {
  local architecture="$1"

  case "${architecture}" in
    amd64) printf '%s\n' 'x86_64-unknown-linux-musl' ;;
    arm64) printf '%s\n' 'aarch64-unknown-linux-musl' ;;
    armhf) printf '%s\n' 'armv7-unknown-linux-musleabihf' ;;
    *)
      printf 'unsupported server-node-only Debian architecture: %s\n' \
        "${architecture}" >&2
      exit 1
      ;;
  esac
}

expected_variant_id() {
  local architecture="$1"

  case "${architecture}" in
    amd64) printf '%s\n' 'x86_64-generic' ;;
    arm64) printf '%s\n' 'aarch64-generic' ;;
    armhf) printf '%s\n' 'armv7-generic' ;;
    *)
      printf 'unsupported server-node-only Debian architecture: %s\n' \
        "${architecture}" >&2
      exit 1
      ;;
  esac
}

workspace_version() {
  awk '
    /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' "${ROOT_DIR}/Cargo.toml"
}

validate_server_node_binary() {
  local binary_path="$1"
  local expected_machine actual_machine

  expected_machine="$(expected_elf_machine "${TARGET_ARCH}")"
  actual_machine="$(readelf -h "${binary_path}" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p')"
  if [[ "${actual_machine}" != "${expected_machine}" ]]; then
    printf 'prebuilt server-node ELF machine is %s, expected %s for %s\n' \
      "${actual_machine:-unknown}" "${expected_machine}" "${TARGET_ARCH}" >&2
    exit 1
  fi

  if readelf -l "${binary_path}" | grep -Fq 'Requesting program interpreter' || \
    readelf -d "${binary_path}" 2>/dev/null | grep -Fq '(NEEDED)'; then
    printf 'prebuilt server-node must be a fully static musl executable: %s\n' \
      "${binary_path}" >&2
    exit 1
  fi
}

extract_static_server_node_artifact() {
  local archive_path="$1"
  local checksum_path archive_dir archive_base archive_root archive_entry archive_member expected_entry found_expected_entry
  local expected_target expected_variant expected_revision expected_version
  local -a archive_entries archive_members expected_entries

  [[ -f "${archive_path}" ]] || {
    printf 'static server-node artifact not found: %s\n' "${archive_path}" >&2
    exit 1
  }
  checksum_path="${archive_path}.sha256"
  [[ -f "${checksum_path}" ]] || {
    printf 'static server-node artifact checksum not found: %s\n' "${checksum_path}" >&2
    exit 1
  }

  archive_path="$(cd "$(dirname "${archive_path}")" && pwd)/$(basename "${archive_path}")"
  checksum_path="${archive_path}.sha256"
  archive_dir="$(dirname "${archive_path}")"
  archive_base="$(basename "${archive_path}")"
  (
    cd "${archive_dir}"
    sha256sum -c "$(basename "${checksum_path}")"
  )

  mapfile -t archive_entries < <(tar -tzf "${archive_path}")
  if ((${#archive_entries[@]} != 4)); then
    printf 'static server-node artifact has an unexpected layout: %s\n' \
      "${archive_path}" >&2
    exit 1
  fi
  archive_root="${archive_entries[0]%%/*}"
  # The artifact is supplied from CI, so do not derive a trusted extraction
  # path from an archive member until it is proven to be a single safe name.
  if [[ ! "${archive_root}" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] || \
    [[ "${archive_root}" == "." || "${archive_root}" == ".." ]]; then
    printf 'static server-node artifact has an unexpected layout: %s\n' \
      "${archive_path}" >&2
    exit 1
  fi
  expected_entries=(
    "${archive_root}/"
    "${archive_root}/ironmesh-server-node"
    "${archive_root}/SHA256SUMS"
    "${archive_root}/build-metadata.json"
  )
  for archive_entry in "${archive_entries[@]}"; do
    found_expected_entry=false
    for expected_entry in "${expected_entries[@]}"; do
      if [[ "${archive_entry}" == "${expected_entry}" ]]; then
        found_expected_entry=true
        break
      fi
    done
    if [[ "${found_expected_entry}" == false ]]; then
      printf 'static server-node artifact has an unexpected layout: %s\n' \
        "${archive_path}" >&2
      exit 1
    fi
  done

  # Reject symbolic links, hard links, devices, and other special members
  # before extraction. A safe pathname alone is insufficient because tar can
  # make a link resolve outside the verified artifact tree.
  mapfile -t archive_members < <(tar -tvzf "${archive_path}")
  if ((${#archive_members[@]} != ${#archive_entries[@]})); then
    printf 'static server-node artifact has an unexpected layout: %s\n' \
      "${archive_path}" >&2
    exit 1
  fi
  for archive_entry in "${archive_entries[@]}"; do
    archive_member="${archive_members[0]}"
    archive_members=("${archive_members[@]:1}")
    if [[ "${archive_entry}" == "${archive_root}/" ]]; then
      [[ "${archive_member}" == d* ]] || {
        printf 'static server-node artifact has an unexpected member type: %s\n' \
          "${archive_path}" >&2
        exit 1
      }
    else
      [[ "${archive_member}" == -* ]] || {
        printf 'static server-node artifact has an unexpected member type: %s\n' \
          "${archive_path}" >&2
        exit 1
      }
    fi
  done

  STATIC_ARTIFACT_TEMP_DIR="$(mktemp -d)"
  tar --no-same-owner --no-same-permissions -xzf "${archive_path}" -C "${STATIC_ARTIFACT_TEMP_DIR}"
  (
    cd "${STATIC_ARTIFACT_TEMP_DIR}/${archive_root}"
    sha256sum -c SHA256SUMS
  )

  expected_target="$(expected_rust_target "${TARGET_ARCH}")"
  expected_variant="$(expected_variant_id "${TARGET_ARCH}")"
  expected_revision="$(git -C "${ROOT_DIR}" rev-parse HEAD)"
  expected_version="$(workspace_version)"
  python3 - \
    "${STATIC_ARTIFACT_TEMP_DIR}/${archive_root}/build-metadata.json" \
    "${expected_target}" "${expected_variant}" "${expected_revision}" "${expected_version}" <<'PY'
import json
import sys

metadata_path, expected_target, expected_variant, expected_revision, expected_version = sys.argv[1:]
with open(metadata_path, encoding="utf-8") as metadata_file:
    metadata = json.load(metadata_file)

expected = {
    "schema_version": 1,
    "binary": "ironmesh-server-node",
    "rust_target": expected_target,
    "variant_id": expected_variant,
    "target_cpu": "generic",
    "git_revision": expected_revision,
    "package_version": expected_version,
}
for key, value in expected.items():
    if metadata.get(key) != value:
        raise SystemExit(
            f"static server-node artifact metadata has {key}={metadata.get(key)!r}; "
            f"expected {value!r}"
        )
if metadata.get("source_dirty") is not False:
    raise SystemExit("static server-node artifact was built from a dirty checkout")
PY

  PREBUILT_SERVER_NODE="${STATIC_ARTIFACT_TEMP_DIR}/${archive_root}/ironmesh-server-node"
  validate_server_node_binary "${PREBUILT_SERVER_NODE}"
}

cleanup() {
  if [[ -n "${STATIC_ARTIFACT_TEMP_DIR}" ]]; then
    rm -rf "${STATIC_ARTIFACT_TEMP_DIR}"
  fi
}

trap cleanup EXIT

check_build_dependencies() {
  local output

  local -a profile_args=()

  if [[ "${SERVER_NODE_ONLY}" == true ]]; then
    profile_args=(-Pserver-node-only,cross -a "${TARGET_ARCH}")
  fi

  if output="$(cd "${ROOT_DIR}" && dpkg-checkbuilddeps "${profile_args[@]}" 2>&1)"; then
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
    --suite)
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--suite requires a suite name' >&2
        exit 1
      }
      TARGET_SUITE="$2"
      shift 2
      ;;
    --suite=*)
      TARGET_SUITE="${1#*=}"
      shift
      ;;
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
    --prebuilt-server-node)
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--prebuilt-server-node requires a file' >&2
        exit 1
      }
      PREBUILT_SERVER_NODE="$2"
      shift 2
      ;;
    --static-server-node-artifact)
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--static-server-node-artifact requires an archive path' >&2
        exit 1
      }
      STATIC_SERVER_NODE_ARTIFACT="$2"
      shift 2
      ;;
    --server-node-only)
      SERVER_NODE_ONLY=true
      RUN_PREPARE=false
      shift
      ;;
    --arch)
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--arch requires a Debian architecture' >&2
        exit 1
      }
      TARGET_ARCH="$2"
      shift 2
      ;;
    --arch=*)
      TARGET_ARCH="${1#*=}"
      shift
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

if [[ -n "${PREBUILT_BINARIES_DIR}" && ( -n "${PREBUILT_SERVER_NODE}" || -n "${STATIC_SERVER_NODE_ARTIFACT}" ) ]]; then
  printf '%s\n' '--prebuilt-binaries cannot be combined with a prebuilt server-node artifact' >&2
  exit 1
fi

if [[ -n "${PREBUILT_SERVER_NODE}" && -n "${STATIC_SERVER_NODE_ARTIFACT}" ]]; then
  printf '%s\n' '--prebuilt-server-node and --static-server-node-artifact are mutually exclusive' >&2
  exit 1
fi

require_command git
require_command dpkg-buildpackage
require_command dpkg-architecture
require_command dpkg-parsechangelog
require_command dpkg

NATIVE_SUITE="$(native_suite)"
if [[ -z "${TARGET_SUITE}" ]]; then
  TARGET_SUITE="${NATIVE_SUITE}"
fi
validate_suite_name "${TARGET_SUITE}"

if [[ -z "${TARGET_ARCH}" ]]; then
  TARGET_ARCH="$(dpkg --print-architecture)"
fi
validate_architecture "${TARGET_ARCH}"
TARGET_ARCH="$(dpkg-architecture -a "${TARGET_ARCH}" -qDEB_HOST_ARCH)"

if [[ "${TARGET_SUITE}" != "${NATIVE_SUITE}" ]]; then
  printf 'refusing to build packages for %s on a %s builder\n' \
    "${TARGET_SUITE}" "${NATIVE_SUITE}" >&2
  printf '%s\n' \
    'Build natively for the target suite so debhelper-generated package dependencies remain installable.' >&2
  exit 1
fi

log "building packages for native suite ${TARGET_SUITE}"

if [[ "${SERVER_NODE_ONLY}" == true ]]; then
  if [[ -n "${PREBUILT_BINARIES_DIR}" ]]; then
    printf '%s\n' '--server-node-only requires a single server-node artifact, not --prebuilt-binaries' >&2
    exit 1
  fi
  if [[ -z "${PREBUILT_SERVER_NODE}" && -z "${STATIC_SERVER_NODE_ARTIFACT}" ]]; then
    printf '%s\n' '--server-node-only requires --prebuilt-server-node or --static-server-node-artifact' >&2
    exit 1
  fi
  require_command readelf
  require_command grep
  if [[ -n "${STATIC_SERVER_NODE_ARTIFACT}" ]]; then
    require_command git
    require_command mktemp
    require_command python3
    require_command sha256sum
    require_command tar
  fi
  DPKG_BUILD_ARGS=(-Pserver-node-only,cross -a "${TARGET_ARCH}" "${DPKG_BUILD_ARGS[@]}")
elif [[ "${TARGET_ARCH}" != "$(dpkg --print-architecture)" ]]; then
  printf '%s\n' '--arch is supported only with --server-node-only' >&2
  exit 1
fi

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
  "${ROOT_DIR}/scripts/sync-debian-version.sh" --suite "${TARGET_SUITE}"

  if [[ -n "${PREBUILT_SERVER_NODE}" ]]; then
    [[ -x "${PREBUILT_SERVER_NODE}" ]] || {
      printf 'prebuilt server-node executable not found: %s\n' \
        "${PREBUILT_SERVER_NODE}" >&2
      exit 1
    }
    PREBUILT_SERVER_NODE="$(cd "$(dirname "${PREBUILT_SERVER_NODE}")" && pwd)/$(basename "${PREBUILT_SERVER_NODE}")"
    export IRONMESH_PREBUILT_SERVER_NODE_BIN="${PREBUILT_SERVER_NODE}"
    export IRONMESH_USE_PREBUILT_SERVER_NODE=1
    if [[ "${SERVER_NODE_ONLY}" == true ]]; then
      validate_server_node_binary "${PREBUILT_SERVER_NODE}"
    fi
    log "packaging prebuilt server node from ${PREBUILT_SERVER_NODE}"
  elif [[ -n "${STATIC_SERVER_NODE_ARTIFACT}" ]]; then
    extract_static_server_node_artifact "${STATIC_SERVER_NODE_ARTIFACT}"
    export IRONMESH_PREBUILT_SERVER_NODE_BIN="${PREBUILT_SERVER_NODE}"
    export IRONMESH_USE_PREBUILT_SERVER_NODE=1
    log "packaging verified static server node from ${STATIC_SERVER_NODE_ARTIFACT}"
  fi

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
ARCH="${TARGET_ARCH}"
CHANGES_PATH="${ARTIFACT_DIR}/${SOURCE_NAME}_${VERSION}_${ARCH}.changes"
BUILDINFO_PATH="${ARTIFACT_DIR}/${SOURCE_NAME}_${VERSION}_${ARCH}.buildinfo"
if [[ "${SERVER_NODE_ONLY}" == true ]]; then
  PACKAGE_PATHS=("${ARTIFACT_DIR}/ironmesh-server-node_${VERSION}_${ARCH}.deb")
else
  PACKAGE_PATHS=(
    "${ARTIFACT_DIR}/ironmesh-client_${VERSION}_${ARCH}.deb"
    "${ARTIFACT_DIR}/ironmesh-server-node_${VERSION}_${ARCH}.deb"
    "${ARTIFACT_DIR}/ironmesh-server-node-map-tools_${VERSION}_${ARCH}.deb"
    "${ARTIFACT_DIR}/ironmesh-rendezvous-service_${VERSION}_${ARCH}.deb"
  )
fi

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
