#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

ZIG_VERSION="0.16.0"
CARGO_ZIGBUILD_VERSION="0.23.0"
ZIG_CACHE_DIR="${IRONMESH_ZIG_CACHE_DIR:-${HOME}/.cache/ironmesh-build-tools}"

PACKAGE_NAME="server-node"
BINARY_NAME="berrykeep-server-node"
TARGET_TRIPLE=""
TARGET_CPU=""
VARIANT_ID=""
OUTPUT_DIR="${ROOT_DIR}/target/static-server-node"
DEPLOY_TARGET=""
RUN_SMOKE_TEST="auto"

log() {
  printf '[build-static-server-node] %s\n' "$*"
}

fail() {
  printf '[build-static-server-node] error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

usage() {
  cat <<'EOF'
Build and verify a fully static musl BerryKeep Server Node artifact.

Usage:
  ./scripts/build-static-server-node.sh --target TRIPLE [options]

Required:
  --target TRIPLE       One of x86_64-unknown-linux-musl,
                        aarch64-unknown-linux-musl, or
                        armv7-unknown-linux-musleabihf.

Options:
  --variant-id ID       Artifact variant ID. Defaults to the generic ID for
                        the selected target.
  --target-cpu CPU      Optional Rust target-cpu value for a specialized
                        complete binary. Generic artifacts leave this unset.
  --output-dir DIR      Artifact output directory. Defaults to
                        target/static-server-node.
  --run-smoke MODE      Version smoke test mode: auto, always, or never.
                        Defaults to auto, which runs only on a matching host.
  --deploy TARGET       Copy the verified binary to an scp target.
  -h, --help            Show this help text.

The output is a tar.gz archive containing the executable, SHA256SUMS, and
build-metadata.json, plus a checksum for the archive itself. The build fails if
the ELF binary requests a dynamic interpreter or contains a DT_NEEDED entry.

Environment:
  IRONMESH_ZIG_CACHE_DIR      Cache directory for the verified Zig toolchain.
  IRONMESH_PREBUILT_WEB_DIR   Optional prebuilt server-admin/client-ui assets
                               consumed by the existing Rust build scripts.
EOF
}

parse_args() {
  while (($# > 0)); do
    case "$1" in
      --target)
        (($# >= 2)) || fail '--target requires a value'
        TARGET_TRIPLE="$2"
        shift 2
        ;;
      --target=*)
        TARGET_TRIPLE="${1#*=}"
        shift
        ;;
      --variant-id)
        (($# >= 2)) || fail '--variant-id requires a value'
        VARIANT_ID="$2"
        shift 2
        ;;
      --variant-id=*)
        VARIANT_ID="${1#*=}"
        shift
        ;;
      --target-cpu)
        (($# >= 2)) || fail '--target-cpu requires a value'
        TARGET_CPU="$2"
        shift 2
        ;;
      --target-cpu=*)
        TARGET_CPU="${1#*=}"
        shift
        ;;
      --output-dir)
        (($# >= 2)) || fail '--output-dir requires a value'
        OUTPUT_DIR="$2"
        shift 2
        ;;
      --output-dir=*)
        OUTPUT_DIR="${1#*=}"
        shift
        ;;
      --run-smoke)
        (($# >= 2)) || fail '--run-smoke requires a value'
        RUN_SMOKE_TEST="$2"
        shift 2
        ;;
      --run-smoke=*)
        RUN_SMOKE_TEST="${1#*=}"
        shift
        ;;
      --deploy)
        (($# >= 2)) || fail '--deploy requires a value'
        DEPLOY_TARGET="$2"
        shift 2
        ;;
      --deploy=*)
        DEPLOY_TARGET="${1#*=}"
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
}

configure_target() {
  case "${TARGET_TRIPLE}" in
    x86_64-unknown-linux-musl)
      DEFAULT_VARIANT_ID="x86_64-generic"
      EXPECTED_ELF_MACHINE="Advanced Micro Devices X86-64"
      ;;
    aarch64-unknown-linux-musl)
      DEFAULT_VARIANT_ID="aarch64-generic"
      EXPECTED_ELF_MACHINE="AArch64"
      ;;
    armv7-unknown-linux-musleabihf)
      DEFAULT_VARIANT_ID="armv7-generic"
      EXPECTED_ELF_MACHINE="ARM"
      ;;
    '')
      fail '--target is required'
      ;;
    *)
      fail "unsupported target: ${TARGET_TRIPLE}"
      ;;
  esac

  VARIANT_ID="${VARIANT_ID:-${DEFAULT_VARIANT_ID}}"
  [[ "${VARIANT_ID}" =~ ^[a-z0-9][a-z0-9._-]*$ ]] \
    || fail "invalid variant ID: ${VARIANT_ID}"
  if [[ -n "${TARGET_CPU}" ]]; then
    [[ "${TARGET_CPU}" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]] \
      || fail "invalid target CPU: ${TARGET_CPU}"
  fi

  case "${RUN_SMOKE_TEST}" in
    auto|always|never) ;;
    *) fail "invalid --run-smoke mode: ${RUN_SMOKE_TEST}" ;;
  esac

  [[ -n "${OUTPUT_DIR}" && "${OUTPUT_DIR}" != "/" ]] \
    || fail "refusing unsafe output directory: ${OUTPUT_DIR}"
}

zig_host_details() {
  case "$(uname -m)" in
    x86_64)
      ZIG_HOST="x86_64-linux"
      ZIG_SHA256="70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00"
      ;;
    aarch64|arm64)
      ZIG_HOST="aarch64-linux"
      ZIG_SHA256="ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17"
      ;;
    *)
      fail "Zig bootstrap supports only x86_64 and aarch64 Linux hosts"
      ;;
  esac

  ZIG_ARCHIVE="zig-${ZIG_HOST}-${ZIG_VERSION}.tar.xz"
  ZIG_DIR="${ZIG_CACHE_DIR}/zig-${ZIG_HOST}-${ZIG_VERSION}"
  ZIG_URL="https://ziglang.org/download/${ZIG_VERSION}/${ZIG_ARCHIVE}"
}

install_zig() {
  zig_host_details

  if command -v zig >/dev/null 2>&1 && [[ "$(zig version)" == "${ZIG_VERSION}" ]]; then
    log "using Zig ${ZIG_VERSION} from $(command -v zig)"
    return
  fi

  if [[ -x "${ZIG_DIR}/zig" ]]; then
    export PATH="${ZIG_DIR}:${PATH}"
    log "using cached Zig ${ZIG_VERSION} from ${ZIG_DIR}"
    return
  fi

  log "downloading Zig ${ZIG_VERSION} for ${ZIG_HOST}"
  mkdir -p "${ZIG_CACHE_DIR}"
  local download_dir tarball
  download_dir="$(mktemp -d)"
  tarball="${download_dir}/${ZIG_ARCHIVE}"
  trap 'rm -rf "${download_dir}"' RETURN

  curl --proto '=https' --tlsv1.2 -fsSL "${ZIG_URL}" -o "${tarball}"
  printf '%s  %s\n' "${ZIG_SHA256}" "${tarball}" | sha256sum -c -
  tar -C "${ZIG_CACHE_DIR}" -xf "${tarball}"

  [[ -x "${ZIG_DIR}/zig" ]] || fail "Zig extraction did not produce ${ZIG_DIR}/zig"
  rm -rf "${download_dir}"
  trap - RETURN
  export PATH="${ZIG_DIR}:${PATH}"
  log "installed Zig ${ZIG_VERSION} under ${ZIG_DIR}"
}

install_cargo_zigbuild() {
  if command -v cargo-zigbuild >/dev/null 2>&1; then
    if [[ "$(cargo-zigbuild --version)" == "cargo-zigbuild ${CARGO_ZIGBUILD_VERSION}" ]]; then
      log "using cargo-zigbuild ${CARGO_ZIGBUILD_VERSION} from $(command -v cargo-zigbuild)"
      return
    fi
    log "replacing cargo-zigbuild with pinned version ${CARGO_ZIGBUILD_VERSION}"
  fi

  log "installing cargo-zigbuild ${CARGO_ZIGBUILD_VERSION}"
  cargo install cargo-zigbuild \
    --version "${CARGO_ZIGBUILD_VERSION}" \
    --locked \
    --force
}

install_rust_target() {
  if (
    cd "${ROOT_DIR}"
    rustup target list --installed | grep -Fx "${TARGET_TRIPLE}" >/dev/null
  ); then
    return
  fi

  log "adding Rust target ${TARGET_TRIPLE}"
  (
    cd "${ROOT_DIR}"
    rustup target add "${TARGET_TRIPLE}"
  )
}

target_rustflags_variable() {
  local normalized_target
  normalized_target="${TARGET_TRIPLE^^}"
  normalized_target="${normalized_target//-/_}"
  printf 'CARGO_TARGET_%s_RUSTFLAGS\n' "${normalized_target}"
}

build_binary() {
  local rustflags_variable rustflags_value
  rustflags_variable="$(target_rustflags_variable)"
  rustflags_value=""
  if [[ -n "${TARGET_CPU}" ]]; then
    rustflags_value="-C target-cpu=${TARGET_CPU}"
  fi

  if [[ -n "${RUSTFLAGS:-}" ]]; then
    fail 'RUSTFLAGS must be empty; use --target-cpu for deterministic target-specific flags'
  fi
  if [[ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]]; then
    fail 'CARGO_ENCODED_RUSTFLAGS must be empty; use --target-cpu for deterministic target-specific flags'
  fi

  log "building ${BINARY_NAME} for ${TARGET_TRIPLE} (${VARIANT_ID})"
  (
    cd "${ROOT_DIR}"
    env "${rustflags_variable}=${rustflags_value}" \
      cargo zigbuild \
        --locked \
        --config profile.release.panic='"abort"' \
        --target "${TARGET_TRIPLE}" \
        --release \
        -p "${PACKAGE_NAME}" \
        --bin "${BINARY_NAME}"
  )
}

verify_static_elf() {
  local binary_path="$1"
  local actual_machine program_headers dynamic_section

  actual_machine="$(readelf -h "${binary_path}" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p')"
  [[ "${actual_machine}" == "${EXPECTED_ELF_MACHINE}" ]] \
    || fail "unexpected ELF machine '${actual_machine}', expected '${EXPECTED_ELF_MACHINE}'"

  program_headers="$(readelf -l "${binary_path}")"
  if grep -Fq 'Requesting program interpreter' <<<"${program_headers}"; then
    fail 'binary requests a dynamic program interpreter'
  fi
  dynamic_section="$(readelf -d "${binary_path}" 2>/dev/null || true)"
  if grep -Fq '(NEEDED)' <<<"${dynamic_section}"; then
    fail 'binary contains a dynamic DT_NEEDED dependency'
  fi

  log "verified static ELF: $(file -b "${binary_path}")"
}

host_matches_target() {
  case "$(uname -m):${TARGET_TRIPLE}" in
    x86_64:x86_64-unknown-linux-musl) return 0 ;;
    aarch64:aarch64-unknown-linux-musl) return 0 ;;
    arm64:aarch64-unknown-linux-musl) return 0 ;;
    armv7l:armv7-unknown-linux-musleabihf) return 0 ;;
    *) return 1 ;;
  esac
}

run_version_smoke_test() {
  local binary_path="$1"

  if [[ "${RUN_SMOKE_TEST}" == "never" ]]; then
    log 'skipping version smoke test by request'
    return
  fi
  if [[ "${RUN_SMOKE_TEST}" == "auto" ]] && ! host_matches_target; then
    log "skipping version smoke test for cross-built ${TARGET_TRIPLE} binary"
    return
  fi

  log 'running version smoke test'
  "${binary_path}" --version
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

cargo_target_directory() {
  (
    cd "${ROOT_DIR}"
    cargo metadata --locked --no-deps --format-version 1
  ) | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])'
}

write_artifact() {
  local binary_path="$1"
  local version revision source_dirty target_cpu artifact_basename artifact_path
  local staging_root staging_dir binary_sha archive_sha

  version="$(workspace_version)"
  [[ -n "${version}" ]] || fail 'failed to read workspace package version'
  revision="$(git -C "${ROOT_DIR}" rev-parse HEAD)"
  source_dirty=false
  if [[ -n "$(git -C "${ROOT_DIR}" status --short --untracked-files=normal)" ]]; then
    source_dirty=true
  fi
  target_cpu="${TARGET_CPU:-generic}"

  artifact_basename="${BINARY_NAME}-${version}-${VARIANT_ID}"
  mkdir -p "${OUTPUT_DIR}"
  artifact_path="${OUTPUT_DIR%/}/${artifact_basename}.tar.gz"
  staging_root="$(mktemp -d)"
  staging_dir="${staging_root}/${artifact_basename}"
  trap 'rm -rf "${staging_root}"' RETURN
  mkdir -p "${staging_dir}"

  install -m 0755 "${binary_path}" "${staging_dir}/${BINARY_NAME}"
  binary_sha="$(sha256sum "${staging_dir}/${BINARY_NAME}" | awk '{print $1}')"
  printf '%s  %s\n' "${binary_sha}" "${BINARY_NAME}" > "${staging_dir}/SHA256SUMS"
  cat > "${staging_dir}/build-metadata.json" <<EOF
{
  "schema_version": 1,
  "artifact": "${artifact_basename}",
  "binary": "${BINARY_NAME}",
  "package_version": "${version}",
  "git_revision": "${revision}",
  "source_dirty": ${source_dirty},
  "variant_id": "${VARIANT_ID}",
  "rust_target": "${TARGET_TRIPLE}",
  "target_cpu": "${target_cpu}",
  "sha256": "${binary_sha}"
}
EOF

  tar -C "${staging_root}" -czf "${artifact_path}" "${artifact_basename}"
  archive_sha="$(sha256sum "${artifact_path}" | awk '{print $1}')"
  printf '%s  %s\n' "${archive_sha}" "$(basename "${artifact_path}")" \
    > "${artifact_path}.sha256"
  rm -rf "${staging_root}"
  trap - RETURN
  log "wrote ${artifact_path}"
  log "wrote ${artifact_path}.sha256"
}

main() {
  parse_args "$@"
  configure_target

  require_command awk
  require_command cargo
  require_command curl
  require_command file
  require_command git
  require_command grep
  require_command readelf
  require_command python3
  require_command rustup
  require_command sed
  require_command sha256sum
  require_command tar

  install_zig
  install_cargo_zigbuild
  install_rust_target
  build_binary

  local cargo_target_dir binary_path
  cargo_target_dir="$(cargo_target_directory)"
  binary_path="${cargo_target_dir}/${TARGET_TRIPLE}/release/${BINARY_NAME}"
  [[ -x "${binary_path}" ]] || fail "expected executable not found: ${binary_path}"
  verify_static_elf "${binary_path}"
  run_version_smoke_test "${binary_path}"
  write_artifact "${binary_path}"

  if [[ -n "${DEPLOY_TARGET}" ]]; then
    require_command scp
    log "deploying verified binary to ${DEPLOY_TARGET}"
    scp "${binary_path}" "${DEPLOY_TARGET}"
  fi
}

main "$@"
