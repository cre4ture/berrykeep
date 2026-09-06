#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACT_DIR="$(cd "${ROOT_DIR}/.." && pwd)"

REPO_DIR="${APT_REPO_DIR:-${ROOT_DIR}/target/apt-repo}"
SUITE="${APT_REPO_SUITE:-}"
CODENAME="${APT_REPO_CODENAME:-}"
COMPONENT="${APT_REPO_COMPONENT:-main}"
DEFAULT_ARCH="${APT_REPO_ARCH:-$(dpkg --print-architecture)}"
ORIGIN="${APT_REPO_ORIGIN:-BerryKeep}"
LABEL="${APT_REPO_LABEL:-BerryKeep}"
DESCRIPTION="${APT_REPO_DESCRIPTION:-BerryKeep Debian package repository}"
SIGNING_KEY="${APT_REPO_SIGN_KEY:-${DEBUILD_KEYID:-${DEBSIGN_KEYID:-}}}"
GPG_PASSPHRASE="${APT_REPO_GPG_PASSPHRASE:-}"
IMPORT_REMOTE="${APT_REPO_IMPORT_REMOTE:-}"
SIGN_REPO=true
SERVER_NODE_ONLY=false
SERVER_NODE_MATRIX=""
DEB_PATHS=()
REQUESTED_ARCHES=()
INDEX_ARCHES=()

log() {
  printf '[build-apt-repository] %s\n' "$*"
}

usage() {
  cat <<'EOF'
Build a simple signed apt repository from locally built BerryKeep .deb packages.

Usage:
  ./scripts/build-apt-repository.sh [options] [--] [package.deb ...]

Options:
  --repo-dir DIR       Output repository directory. Defaults to target/apt-repo.
  --suite NAME         Apt suite/distribution. Defaults to APT_REPO_SUITE or
                       the local VERSION_CODENAME. Use trixie for packages
                       built natively on Debian Trixie.
  --codename NAME      Release codename. Defaults to the suite name.
  --component NAME     Apt component. Defaults to main.
  --arch ARCH          Architecture to update. May be passed more than once.
                       Without explicit package paths, defaults to
                       dpkg --print-architecture.
  --import-remote SRC  Rsync source for the published repository to import
                       before updating it, for example
                       creature@creax.de:/home/creature/html/apt/ironmesh.
  --sign-key KEY       GPG key ID or fingerprint used for Release signing.
  --server-node-only   Publish berrykeep-server-node and its Ironmesh
                       transition package. With no explicit .deb path,
                       expects both packages.
  --server-node-matrix FILE
                       Sign each server-node-only matrix row. Each non-comment
                       row is: SUITE ARCH PACKAGE_PATH. Relative package paths
                       are resolved from FILE's directory. The matching
                       ironmesh-server-node transition package is discovered
                       beside each primary package. The existing remote
                       repository is imported only before the first row.
  --no-sign            Build repository metadata without signing it.
  -h, --help           Show this help text.

Environment defaults:
  APT_REPO_DIR, APT_REPO_SUITE, APT_REPO_CODENAME, APT_REPO_COMPONENT,
  APT_REPO_ARCH, APT_REPO_ORIGIN, APT_REPO_LABEL, APT_REPO_DESCRIPTION,
  APT_REPO_IMPORT_REMOTE, APT_REPO_SIGN_KEY, APT_REPO_GPG_PASSPHRASE,
  DEBUILD_KEYID, DEBSIGN_KEYID.

If no .deb paths are passed, the script expects the current changelog version
artifacts in the parent directory of the checkout. Run
./scripts/build-local-debs.sh first to create them.
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
  local os_suite

  if [[ ! -r /etc/os-release ]]; then
    printf '%s\n' 'unable to identify the native build suite: /etc/os-release is not readable' >&2
    exit 1
  fi

  # shellcheck disable=SC1091
  . /etc/os-release
  os_suite="${VERSION_CODENAME:-}"
  if [[ -z "${os_suite}" ]]; then
    printf '%s\n' 'unable to identify the native build suite: VERSION_CODENAME is empty' >&2
    exit 1
  fi

  printf '%s\n' "${os_suite}"
}

validate_release_name() {
  local label="$1"
  local value="$2"

  if [[ ! "${value}" =~ ^[a-z0-9][a-z0-9.-]*$ ]]; then
    printf 'invalid %s: %s\n' "${label}" "${value}" >&2
    exit 1
  fi
}

verify_signed_release() {
  local inrelease_path="$1"
  local release_path="$2"
  local signature_path="$3"

  if ! grep -Fxq -- '-----BEGIN PGP SIGNED MESSAGE-----' "${inrelease_path}" || \
    ! grep -Fxq -- '-----BEGIN PGP SIGNATURE-----' "${inrelease_path}" || \
    ! grep -Fxq -- '-----END PGP SIGNATURE-----' "${inrelease_path}"; then
    printf 'generated InRelease is missing an inline OpenPGP signature: %s\n' \
      "${inrelease_path}" >&2
    exit 1
  fi

  if ! gpg --batch --verify "${inrelease_path}" >/dev/null 2>&1; then
    printf 'failed to verify generated InRelease: %s\n' "${inrelease_path}" >&2
    exit 1
  fi

  if ! gpg --batch --verify "${signature_path}" "${release_path}" >/dev/null 2>&1; then
    printf 'failed to verify generated Release.gpg: %s\n' "${signature_path}" >&2
    exit 1
  fi
}

sign_release() {
  local mode="$1"
  local output_path="$2"
  local input_path="$3"
  local -a sign_args=(
    --batch
    --yes
    --local-user "${SIGNING_KEY}"
    --digest-algo SHA256
  )

  if [[ -n "${GPG_PASSPHRASE}" ]]; then
    sign_args+=(--pinentry-mode loopback --passphrase-fd 0)
  fi

  case "${mode}" in
    clearsign)
      sign_args+=(--clearsign -o "${output_path}" "${input_path}")
      ;;
    detached)
      sign_args+=(--armor --detach-sign -o "${output_path}" "${input_path}")
      ;;
    *)
      printf 'unsupported apt Release signing mode: %s\n' "${mode}" >&2
      exit 1
      ;;
  esac

  if [[ -n "${GPG_PASSPHRASE}" ]]; then
    printf '%s\n' "${GPG_PASSPHRASE}" | \
      gpg "${sign_args[@]}"
  else
    gpg "${sign_args[@]}"
  fi
}

run_server_node_matrix() {
  local matrix_path="$1"
  local matrix_dir raw_line suite architecture package_path transition_package_path extra
  local package_version transition_package_name transition_package_architecture transition_package_version
  local line_number=0 processed_rows=0 import_remote_for_row
  local -a child_args

  [[ -f "${matrix_path}" ]] || {
    printf 'server-node matrix not found: %s\n' "${matrix_path}" >&2
    exit 1
  }
  if [[ -n "${CODENAME}" ]]; then
    printf '%s\n' '--codename cannot be combined with --server-node-matrix; each row uses its suite as codename' >&2
    exit 1
  fi
  if ((${#DEB_PATHS[@]} != 0)); then
    printf '%s\n' '--server-node-matrix cannot be combined with explicit .deb paths' >&2
    exit 1
  fi
  if ((${#REQUESTED_ARCHES[@]} != 0)); then
    printf '%s\n' '--server-node-matrix cannot be combined with --arch; each row declares its architecture' >&2
    exit 1
  fi

  matrix_path="$(cd "$(dirname "${matrix_path}")" && pwd)/$(basename "${matrix_path}")"
  matrix_dir="$(dirname "${matrix_path}")"
  import_remote_for_row="${IMPORT_REMOTE}"

  while IFS= read -r raw_line || [[ -n "${raw_line}" ]]; do
    ((line_number += 1))
    [[ -z "${raw_line//[[:space:]]/}" || "${raw_line}" =~ ^[[:space:]]*# ]] && continue
    ((processed_rows += 1))

    suite=""
    architecture=""
    package_path=""
    extra=""
    read -r suite architecture package_path extra <<<"${raw_line}"
    if [[ -z "${suite}" || -z "${architecture}" || -z "${package_path}" || -n "${extra}" ]]; then
      printf 'invalid server-node matrix row %s in %s; expected: SUITE ARCH PACKAGE_PATH\n' \
        "${line_number}" "${matrix_path}" >&2
      exit 1
    fi
    if [[ "${package_path}" != /* ]]; then
      package_path="${matrix_dir}/${package_path}"
    fi
    package_version="$(dpkg-deb -f "${package_path}" Version)"
    transition_package_path="$(dirname "${package_path}")/ironmesh-server-node_${package_version}_${architecture}.deb"
    [[ -f "${transition_package_path}" ]] || {
      printf 'server-node transition package not found beside %s: %s\n' \
        "${package_path}" "${transition_package_path}" >&2
      exit 1
    }
    transition_package_name="$(dpkg-deb -f "${transition_package_path}" Package)"
    transition_package_architecture="$(dpkg-deb -f "${transition_package_path}" Architecture)"
    transition_package_version="$(dpkg-deb -f "${transition_package_path}" Version)"
    if [[ "${transition_package_name}" != "ironmesh-server-node" || \
      "${transition_package_architecture}" != "${architecture}" || \
      "${transition_package_version}" != "${package_version}" ]]; then
      printf 'invalid server-node transition package: %s\n' "${transition_package_path}" >&2
      exit 1
    fi

    child_args=(
      --repo-dir "${REPO_DIR}"
      --suite "${suite}"
      --component "${COMPONENT}"
      --arch "${architecture}"
      --server-node-only
    )
    if [[ "${SIGN_REPO}" == true ]]; then
      child_args+=(--sign-key "${SIGNING_KEY}")
    else
      child_args+=(--no-sign)
    fi
    if [[ -n "${import_remote_for_row}" ]]; then
      child_args+=(--import-remote "${import_remote_for_row}")
      import_remote_for_row=""
    fi

    log "processing server-node matrix row ${suite}/${architecture}"
    # Child processes inherit environment defaults. Clear the import source so
    # only the first row receives the captured --import-remote value above.
    APT_REPO_IMPORT_REMOTE="" \
      APT_REPO_GPG_PASSPHRASE="${GPG_PASSPHRASE}" \
      "${ROOT_DIR}/scripts/build-apt-repository.sh" "${child_args[@]}" -- \
      "${package_path}" "${transition_package_path}"
  done < "${matrix_path}"

  if ((processed_rows == 0)); then
    printf 'server-node matrix is empty: %s\n' "${matrix_path}" >&2
    exit 1
  fi
}

while (($# > 0)); do
  case "$1" in
    --repo-dir)
      REPO_DIR="$2"
      shift 2
      ;;
    --repo-dir=*)
      REPO_DIR="${1#*=}"
      shift
      ;;
    --suite)
      SUITE="$2"
      shift 2
      ;;
    --suite=*)
      SUITE="${1#*=}"
      shift
      ;;
    --codename)
      CODENAME="$2"
      shift 2
      ;;
    --codename=*)
      CODENAME="${1#*=}"
      shift
      ;;
    --component)
      COMPONENT="$2"
      shift 2
      ;;
    --component=*)
      COMPONENT="${1#*=}"
      shift
      ;;
    --arch)
      REQUESTED_ARCHES+=("$2")
      shift 2
      ;;
    --arch=*)
      REQUESTED_ARCHES+=("${1#*=}")
      shift
      ;;
    --import-remote)
      IMPORT_REMOTE="$2"
      shift 2
      ;;
    --import-remote=*)
      IMPORT_REMOTE="${1#*=}"
      shift
      ;;
    --sign-key)
      SIGNING_KEY="$2"
      shift 2
      ;;
    --sign-key=*)
      SIGNING_KEY="${1#*=}"
      shift
      ;;
    --server-node-only)
      SERVER_NODE_ONLY=true
      shift
      ;;
    --server-node-matrix)
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--server-node-matrix requires a matrix file' >&2
        exit 1
      }
      SERVER_NODE_MATRIX="$2"
      shift 2
      ;;
    --server-node-matrix=*)
      SERVER_NODE_MATRIX="${1#*=}"
      shift
      ;;
    --no-sign)
      SIGN_REPO=false
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      DEB_PATHS+=("$@")
      break
      ;;
    -*)
      printf 'unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 1
      ;;
    *)
      DEB_PATHS+=("$1")
      shift
      ;;
  esac
done

require_command apt-ftparchive
require_command awk
require_command dpkg
require_command dpkg-deb
require_command dpkg-parsechangelog
require_command dpkg-scanpackages
require_command gzip
require_command mktemp
require_command sed

if [[ -n "${SERVER_NODE_MATRIX}" ]]; then
  run_server_node_matrix "${SERVER_NODE_MATRIX}"
  exit 0
fi

if [[ -z "${SUITE}" ]]; then
  SUITE="$(native_suite)"
fi

if [[ -z "${CODENAME}" ]]; then
  CODENAME="${SUITE}"
fi

validate_release_name 'apt suite' "${SUITE}"
validate_release_name 'apt codename' "${CODENAME}"
validate_release_name 'apt component' "${COMPONENT}"

if [[ "${SIGN_REPO}" == true ]]; then
  require_command gpg

  if [[ -z "${SIGNING_KEY}" ]]; then
    printf 'set APT_REPO_SIGN_KEY, DEBUILD_KEYID, or DEBSIGN_KEYID; or pass --sign-key\n' >&2
    exit 1
  fi

  if [[ -z "${GPG_TTY:-}" && -t 0 ]]; then
    export GPG_TTY
    GPG_TTY="$(tty)"
  fi
fi

if [[ -z "${REPO_DIR}" || "${REPO_DIR}" == "/" ]]; then
  printf 'refusing unsafe repository directory: %s\n' "${REPO_DIR}" >&2
  exit 1
fi

if [[ -n "${IMPORT_REMOTE}" ]]; then
  require_command rsync
  log "importing existing repository from ${IMPORT_REMOTE}"
  mkdir -p "${REPO_DIR}"
  rsync -a --delete "${IMPORT_REMOTE%/}/" "${REPO_DIR%/}/"
fi

contains_architecture() {
  local architecture="$1"
  local candidate

  for candidate in "${REQUESTED_ARCHES[@]}"; do
    if [[ "${candidate}" == "${architecture}" ]]; then
      return 0
    fi
  done

  return 1
}

add_architecture() {
  local architecture="$1"

  if ! contains_architecture "${architecture}"; then
    REQUESTED_ARCHES+=("${architecture}")
  fi
}

contains_index_architecture() {
  local architecture="$1"
  local candidate

  for candidate in "${INDEX_ARCHES[@]}"; do
    if [[ "${candidate}" == "${architecture}" ]]; then
      return 0
    fi
  done

  return 1
}

add_index_architecture() {
  local architecture="$1"

  if ! contains_index_architecture "${architecture}"; then
    INDEX_ARCHES+=("${architecture}")
  fi
}

has_server_node_package_for_architecture() {
  local architecture="$1"
  local package_path package_name package_architecture

  for package_path in "${DEB_PATHS[@]}"; do
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    package_architecture="$(dpkg-deb -f "${package_path}" Architecture)"
    if [[ "${package_name}" == "berrykeep-server-node" && "${package_architecture}" == "${architecture}" ]]; then
      return 0
    fi
  done

  return 1
}

has_server_node_transition_package_for_architecture() {
  local architecture="$1"
  local expected_version="$2"
  local package_path package_name package_architecture package_version

  for package_path in "${DEB_PATHS[@]}"; do
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    package_architecture="$(dpkg-deb -f "${package_path}" Architecture)"
    package_version="$(dpkg-deb -f "${package_path}" Version)"
    if [[ "${package_name}" == "ironmesh-server-node" && \
      "${package_architecture}" == "${architecture}" && \
      "${package_version}" == "${expected_version}" ]] && \
      is_server_node_transition_package "${package_path}"; then
      return 0
    fi
  done

  return 1
}

server_node_version_for_architecture() {
  local architecture="$1"
  local package_path package_name package_architecture package_version
  local selected_version=""

  for package_path in "${DEB_PATHS[@]}"; do
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    package_architecture="$(dpkg-deb -f "${package_path}" Architecture)"
    [[ "${package_name}" == "berrykeep-server-node" && "${package_architecture}" == "${architecture}" ]] || continue
    package_version="$(dpkg-deb -f "${package_path}" Version)"
    if [[ -z "${selected_version}" ]] || \
      dpkg --compare-versions "${package_version}" gt "${selected_version}"; then
      selected_version="${package_version}"
    fi
  done

  printf '%s\n' "${selected_version}"
}

pool_package_is_indexed_in_suite() {
  local architecture="$1"
  local pool_rel="$2"
  local package_path="$3"
  local packages_path package_filename

  packages_path="${SUITE_DIR}/${COMPONENT}/binary-${architecture}/Packages"
  package_filename="${pool_rel}/$(basename "${package_path}")"

  if [[ -f "${packages_path}" ]]; then
    grep -Fxq -- "Filename: ${package_filename}" "${packages_path}"
    return
  fi

  if [[ -f "${packages_path}.gz" ]]; then
    grep -Fxq -- "Filename: ${package_filename}" < <(gzip -cd -- "${packages_path}.gz")
    return
  fi

  return 1
}

preserve_legacy_suite_packages() {
  local architecture="$1"
  local pool pool_dir pool_rel package_path package_architecture
  local -a package_pools=(
    "${LEGACY_POOL_DIR}:${LEGACY_POOL_REL}"
    "${PREVIOUS_SUITE_POOL_DIR}:${PREVIOUS_SUITE_POOL_REL}"
  )
  local -a legacy_packages=()

  for pool in "${package_pools[@]}"; do
    pool_dir="${pool%%:*}"
    pool_rel="${pool#*:}"
    legacy_packages=("${pool_dir}"/*_"${architecture}".deb)
    for package_path in "${legacy_packages[@]}"; do
      [[ -f "${package_path}" ]] || continue
      if ! pool_package_is_indexed_in_suite "${architecture}" "${pool_rel}" "${package_path}"; then
        continue
      fi
      cp -f "${package_path}" "${POOL_DIR}/"
    done

    legacy_packages=("${pool_dir}"/*_all.deb)
    for package_path in "${legacy_packages[@]}"; do
      [[ -f "${package_path}" ]] || continue
      package_architecture="$(dpkg-deb -f "${package_path}" Architecture)"
      [[ "${package_architecture}" == "all" ]] || continue
      if ! pool_package_is_indexed_in_suite "${architecture}" "${pool_rel}" "${package_path}"; then
        continue
      fi
      cp -f "${package_path}" "${POOL_DIR}/"
    done
  done
}

prune_replaced_arch_all_packages() {
  local package_path package_name package_architecture

  for package_path in "${DEB_PATHS[@]}"; do
    package_architecture="$(dpkg-deb -f "${package_path}" Architecture)"
    [[ "${package_architecture}" == "all" ]] || continue
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    rm -f "${POOL_DIR}/${package_name}_"*_all.deb
  done
}

is_server_node_transition_package() {
  local package_path="$1"
  local package_name package_architecture depends

  package_name="$(dpkg-deb -f "${package_path}" Package)"
  package_architecture="$(dpkg-deb -f "${package_path}" Architecture)"
  [[ "${package_name}" == "ironmesh-server-node" && "${package_architecture}" != "all" ]] || return 1

  depends="$(dpkg-deb -f "${package_path}" Depends)"
  grep -Eq '(^|, )berrykeep-server-node[[:space:]]*\(>=' <<<"${depends}"
}

prune_server_node_versions() {
  local architecture="$1"
  local package_path package_name package_architecture package_version depends required_version retained
  local retained_version
  local -a retained_berrykeep_versions=()
  local -a retained_ironmesh_versions=()
  local -a retained_transition_versions=()
  local -a retained_versions=()

  # Keep the BerryKeep Server Node version being published. An exact legacy
  # Map Tools dependency protects only the matching Ironmesh Server Node while
  # it is still needed; the compatibility rebuild removes that dependency.
  for package_path in "${DEB_PATHS[@]}"; do
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    package_architecture="$(dpkg-deb -f "${package_path}" Architecture)"
    package_version="$(dpkg-deb -f "${package_path}" Version)"
    if [[ "${package_name}" == "berrykeep-server-node" && "${package_architecture}" == "${architecture}" ]]; then
      retained_berrykeep_versions+=("${package_version}")
    elif [[ "${package_name}" == "ironmesh-server-node" && \
      "${package_architecture}" == "${architecture}" ]] && \
      is_server_node_transition_package "${package_path}"; then
      retained_transition_versions+=("${package_version}")
    fi
  done

  for package_path in "${POOL_DIR}"/*_"${architecture}".deb; do
    [[ -f "${package_path}" ]] || continue
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    [[ "${package_name}" == "ironmesh-server-node-map-tools" ]] || continue
    depends="$(dpkg-deb -f "${package_path}" Depends)"
    required_version="$(sed -n 's/.*ironmesh-server-node[[:space:]]*(=[[:space:]]*\([^)]*\)).*/\1/p' <<<"${depends}" | tr -d '[:space:]')"
    [[ -n "${required_version}" ]] && retained_ironmesh_versions+=("${required_version}")
  done

  for package_path in "${POOL_DIR}"/*_"${architecture}".deb; do
    [[ -f "${package_path}" ]] || continue
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    case "${package_name}" in
      berrykeep-server-node)
        retained_versions=("${retained_berrykeep_versions[@]}")
        ;;
      ironmesh-server-node)
        if is_server_node_transition_package "${package_path}"; then
          retained_versions=("${retained_transition_versions[@]}")
        else
          retained_versions=("${retained_ironmesh_versions[@]}")
        fi
        ;;
      *)
        continue
        ;;
    esac
    package_version="$(dpkg-deb -f "${package_path}" Version)"
    retained=false
    for retained_version in "${retained_versions[@]}"; do
      if [[ "${package_version}" == "${retained_version}" ]]; then
        retained=true
        break
      fi
    done
    if [[ "${retained}" == false ]]; then
      log "removing unreferenced ${package_name} ${package_version} for ${architecture}"
      rm -f "${package_path}"
    fi
  done

}

repack_exact_map_tools_dependencies() {
  local architecture="$1"
  local server_node_version server_node_upstream package_path package_name package_version depends required_version
  local selected_package="" selected_version="" compatibility_path compatibility_depends staging_dir
  local -a exact_map_tools_paths=()

  server_node_version="$(server_node_version_for_architecture "${architecture}")"
  if [[ -z "${server_node_version}" ]]; then
    printf 'server-node-only requested architecture has no input package: %s\n' \
      "${architecture}" >&2
    exit 1
  fi
  server_node_upstream="${server_node_version%%-*}"

  for package_path in "${POOL_DIR}"/*_"${architecture}".deb; do
    [[ -f "${package_path}" ]] || continue
    package_name="$(dpkg-deb -f "${package_path}" Package)"
    [[ "${package_name}" == "ironmesh-server-node-map-tools" ]] || continue
    depends="$(dpkg-deb -f "${package_path}" Depends)"
    required_version="$(sed -n 's/.*ironmesh-server-node[[:space:]]*(=[[:space:]]*\([^)]*\)).*/\1/p' <<<"${depends}" | tr -d '[:space:]')"
    [[ -n "${required_version}" ]] || continue

    package_version="$(dpkg-deb -f "${package_path}" Version)"
    exact_map_tools_paths+=("${package_path}")
    if [[ -z "${selected_package}" ]] || \
      dpkg --compare-versions "${package_version}" gt "${selected_version}"; then
      selected_package="${package_path}"
      selected_version="${package_version}"
    fi
  done

  [[ -n "${selected_package}" ]] || return 0

  if dpkg --compare-versions "${server_node_version}" lt "${selected_version}"; then
    printf 'server-node-only package version %s is older than retained map-tools version %s for %s; publish a full package set instead\n' \
      "${server_node_version}" "${selected_version}" "${architecture}" >&2
    exit 1
  fi
  if dpkg --compare-versions "${server_node_version}" eq "${selected_version}"; then
    return
  fi

  staging_dir="$(mktemp -d)"
  dpkg-deb -R "${selected_package}" "${staging_dir}/map-tools"
  sed -i \
    -e "s/^Version: .*/Version: ${server_node_version}/" \
    -e "s/ironmesh-server-node[[:space:]]*(=[[:space:]]*[^)]*)/berrykeep-server-node (>= ${server_node_upstream})/" \
    "${staging_dir}/map-tools/DEBIAN/control"
  compatibility_path="${POOL_DIR}/ironmesh-server-node-map-tools_${server_node_version}_${architecture}.deb"
  dpkg-deb --root-owner-group --build "${staging_dir}/map-tools" "${compatibility_path}" >/dev/null
  rm -rf "${staging_dir}"

  if ! dpkg-deb -c "${compatibility_path}" | awk '$2 != "root/root" { exit 1 }'; then
    printf 'compatibility package does not preserve root ownership: %s\n' \
      "${compatibility_path}" >&2
    exit 1
  fi

  compatibility_depends="$(dpkg-deb -f "${compatibility_path}" Depends)"
  if ! grep -Fq "berrykeep-server-node (>= ${server_node_upstream})" <<<"${compatibility_depends}"; then
    printf 'failed to relax map-tools dependency in compatibility package: %s\n' \
      "${compatibility_path}" >&2
    exit 1
  fi

  for package_path in "${exact_map_tools_paths[@]}"; do
    rm -f "${package_path}"
  done
  log "repackaged map-tools for ${architecture} with Server Node >= ${server_node_upstream}"
}

if ((${#DEB_PATHS[@]} == 0)); then
  "${ROOT_DIR}/scripts/sync-debian-version.sh" --check
  VERSION="$(cd "${ROOT_DIR}" && dpkg-parsechangelog -SVersion)"
  IMPLICIT_ARCHES=("${REQUESTED_ARCHES[@]}")

  if ((${#IMPLICIT_ARCHES[@]} == 0)); then
    IMPLICIT_ARCHES=("${DEFAULT_ARCH}")
  fi

  REQUESTED_ARCHES=()
  DEB_PATHS=()
  for architecture in "${IMPLICIT_ARCHES[@]}"; do
    add_architecture "${architecture}"
    if [[ "${SERVER_NODE_ONLY}" == true ]]; then
      DEB_PATHS+=(
        "${ARTIFACT_DIR}/berrykeep-server-node_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/ironmesh-server-node_${VERSION}_${architecture}.deb"
      )
    else
      DEB_PATHS+=(
        "${ARTIFACT_DIR}/berrykeep-client_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/berrykeep-server-node_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/berrykeep-server-node-map-tools_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/berrykeep-rendezvous-service_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/ironmesh-client_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/ironmesh-server-node_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/ironmesh-server-node-map-tools_${VERSION}_${architecture}.deb"
        "${ARTIFACT_DIR}/ironmesh-rendezvous-service_${VERSION}_${architecture}.deb"
      )
    fi
  done
fi

for path in "${DEB_PATHS[@]}"; do
  if [[ ! -f "${path}" ]]; then
    printf 'package not found: %s\n' "${path}" >&2
    printf 'Run ./scripts/build-local-debs.sh first, or pass explicit .deb paths.\n' >&2
    exit 1
  fi
done

for path in "${DEB_PATHS[@]}"; do
  package_name="$(dpkg-deb -f "${path}" Package)"
  package_architecture="$(dpkg-deb -f "${path}" Architecture)"
  if [[ "${SERVER_NODE_ONLY}" == true && "${package_name}" != "berrykeep-server-node" && "${package_name}" != "ironmesh-server-node" ]]; then
    printf 'server-node-only repository input must be berrykeep-server-node or its transitional package, got %s: %s\n' \
      "${package_name:-empty}" "${path}" >&2
    exit 1
  fi
  if [[ -z "${package_architecture}" ]]; then
    printf 'package architecture must be a concrete architecture, not %s: %s\n' \
      "${package_architecture:-empty}" "${path}" >&2
    exit 1
  fi

  if [[ "${package_architecture}" == "all" ]]; then
    continue
  fi

  if [[ "${SERVER_NODE_ONLY}" == true && ${#REQUESTED_ARCHES[@]} -ne 0 ]] && \
    ! contains_architecture "${package_architecture}"; then
    printf 'server-node-only package architecture %s does not match requested architecture: %s\n' \
      "${package_architecture}" "${path}" >&2
    exit 1
  fi

  add_architecture "${package_architecture}"
done

if ((${#REQUESTED_ARCHES[@]} == 0)); then
  add_architecture "${DEFAULT_ARCH}"
fi

if [[ "${SERVER_NODE_ONLY}" == true ]]; then
  for architecture in "${REQUESTED_ARCHES[@]}"; do
    if ! has_server_node_package_for_architecture "${architecture}"; then
      printf 'server-node-only requested architecture has no input package: %s\n' \
        "${architecture}" >&2
      exit 1
    fi
    server_node_version="$(server_node_version_for_architecture "${architecture}")"
    if ! has_server_node_transition_package_for_architecture "${architecture}" "${server_node_version}"; then
      printf 'server-node-only requested architecture has no matching transition package: %s (%s)\n' \
        "${architecture}" "${server_node_version}" >&2
      exit 1
    fi
  done
fi

for architecture in "${REQUESTED_ARCHES[@]}"; do
  if [[ ! "${architecture}" =~ ^[a-z0-9][a-z0-9-]*$ ]]; then
    printf 'invalid Debian architecture: %s\n' "${architecture}" >&2
    exit 1
  fi
done

SUITE_DIR="${REPO_DIR}/dists/${SUITE}"
POOL_REL="pool/${COMPONENT}/b/berrykeep/${SUITE}"
POOL_DIR="${REPO_DIR}/${POOL_REL}"
LEGACY_POOL_DIR="${REPO_DIR}/pool/${COMPONENT}/i/ironmesh"
LEGACY_POOL_REL="pool/${COMPONENT}/i/ironmesh"
PREVIOUS_SUITE_POOL_DIR="${LEGACY_POOL_DIR}/${SUITE}"
PREVIOUS_SUITE_POOL_REL="${LEGACY_POOL_REL}/${SUITE}"

log "refreshing ${REPO_DIR}"
mkdir -p "${POOL_DIR}"

# A suite pool is shared by its architecture indexes and can hold
# architecture-independent packages. Capture every existing index before
# replacing any one of them, and preserve all of their legacy package
# references before the first new shared-pool package is published.
mapfile -t existing_index_arches < <(
  find "${SUITE_DIR}/${COMPONENT}" -mindepth 1 -maxdepth 1 -type d -name 'binary-*' -printf '%f\n' 2>/dev/null \
    | sed 's/^binary-//' \
    | sort -u
)
for architecture in "${existing_index_arches[@]}"; do
  add_index_architecture "${architecture}"
done
for architecture in "${REQUESTED_ARCHES[@]}"; do
  add_index_architecture "${architecture}"
done

# Migrate every package still referenced by each suite index before rewriting
# any shared-pool index. This retains architecture siblings on both full and
# server-only publishes, including later matrix rows and packages published
# before suite-scoped pools existed.
for architecture in "${INDEX_ARCHES[@]}"; do
  preserve_legacy_suite_packages "${architecture}"
done

# A publish replaces only architecture-independent packages supplied in this
# invocation. Leave unrelated indexed all packages available to architecture
# siblings that are not being rebuilt.
prune_replaced_arch_all_packages

for architecture in "${REQUESTED_ARCHES[@]}"; do
  if [[ "${SERVER_NODE_ONLY}" != true ]]; then
    rm -f "${POOL_DIR}"/*_"${architecture}".deb
  fi
  rm -rf "${SUITE_DIR}/${COMPONENT}/binary-${architecture}"
  mkdir -p "${SUITE_DIR}/${COMPONENT}/binary-${architecture}"
done

log "copying packages"
cp -f "${DEB_PATHS[@]}" "${POOL_DIR}/"

for architecture in "${REQUESTED_ARCHES[@]}"; do
  if [[ "${SERVER_NODE_ONLY}" == true ]]; then
    repack_exact_map_tools_dependencies "${architecture}"
    prune_server_node_versions "${architecture}"
  fi
done

# Refresh every index captured before replacing an architecture. The suite pool
# is shared, so each index must describe the package files retained for its
# architecture after a partial matrix publish.
for architecture in "${INDEX_ARCHES[@]}"; do
  packages_rel="dists/${SUITE}/${COMPONENT}/binary-${architecture}/Packages"
  log "writing ${packages_rel}"
  (
    cd "${REPO_DIR}"
    dpkg-scanpackages --multiversion --arch "${architecture}" "${POOL_REL}" /dev/null > "${packages_rel}"
    gzip -9cn "${packages_rel}" > "${packages_rel}.gz"
  )
done

RELEASE_ARCHES=("${INDEX_ARCHES[@]}")

RELEASE_ARCHITECTURES="${RELEASE_ARCHES[*]}"

log "writing Release metadata"
rm -f "${SUITE_DIR}/Release" "${SUITE_DIR}/InRelease" "${SUITE_DIR}/Release.gpg"
RELEASE_TMP="$(mktemp)"
trap 'rm -f "${RELEASE_TMP}"' EXIT
apt-ftparchive \
  -o "APT::FTPArchive::Release::Origin=${ORIGIN}" \
  -o "APT::FTPArchive::Release::Label=${LABEL}" \
  -o "APT::FTPArchive::Release::Suite=${SUITE}" \
  -o "APT::FTPArchive::Release::Codename=${CODENAME}" \
  -o "APT::FTPArchive::Release::Architectures=${RELEASE_ARCHITECTURES}" \
  -o "APT::FTPArchive::Release::Components=${COMPONENT}" \
  -o "APT::FTPArchive::Release::Description=${DESCRIPTION}" \
  release "${SUITE_DIR}" > "${RELEASE_TMP}"
mv "${RELEASE_TMP}" "${SUITE_DIR}/Release"

if [[ "${SIGN_REPO}" == true ]]; then
  log "exporting public signing key"
  gpg --armor --export "${SIGNING_KEY}" > "${REPO_DIR}/berrykeep-archive-keyring.asc"
  if [[ ! -s "${REPO_DIR}/berrykeep-archive-keyring.asc" ]]; then
    printf 'failed to export public signing key: %s\n' "${SIGNING_KEY}" >&2
    exit 1
  fi
  cp -f "${REPO_DIR}/berrykeep-archive-keyring.asc" \
    "${REPO_DIR}/ironmesh-archive-keyring.asc"

  log "signing Release metadata with ${SIGNING_KEY}"
  sign_release clearsign "${SUITE_DIR}/InRelease" "${SUITE_DIR}/Release"
  sign_release detached "${SUITE_DIR}/Release.gpg" "${SUITE_DIR}/Release"
  verify_signed_release \
    "${SUITE_DIR}/InRelease" \
    "${SUITE_DIR}/Release" \
    "${SUITE_DIR}/Release.gpg"
else
  log "leaving repository unsigned"
fi

log "repository ready: ${REPO_DIR}"
