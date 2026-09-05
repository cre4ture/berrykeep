#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

REPO_DIR="${APT_REPO_DIR:-${ROOT_DIR}/target/apt-repo}"
DEFAULT_SUITE="${APT_REPO_SUITE:-}"
REMOTE="${APT_REPO_REMOTE:-creature@creax.de}"
REMOTE_DIR="${APT_REPO_REMOTE_DIR:-/home/creature/html/apt/ironmesh}"
REMOTE_URL="${APT_REPO_URL:-https://creax.de/apt/ironmesh}"
DRY_RUN=false
MATRIX_FILE=""
SUITES=()

log() {
  printf '[deploy-apt-repository] %s\n' "$*"
}

usage() {
  cat <<'EOF'
Deploy generated Ironmesh apt repository updates to a static web directory.

Usage:
  ./scripts/deploy-apt-repository.sh [options]

Options:
  --repo-dir DIR     Local repository directory. Defaults to target/apt-repo.
  --suite NAME       Apt suite to deploy. May be repeated. Defaults to
                     APT_REPO_SUITE or the local VERSION_CODENAME. Use trixie
                     for packages built natively on Debian Trixie.
  --server-node-matrix FILE
                     Deploy every suite named by a server-node matrix. Each
                     non-comment row is: SUITE ARCH PACKAGE_PATH. This is the
                     same matrix accepted by build-apt-repository.sh.
  --remote HOST      SSH remote. Defaults to creature@creax.de.
  --remote-dir DIR   Remote web directory. Defaults to /home/creature/html/apt/ironmesh.
  --url URL          Public repository URL printed at the end.
  --dry-run          Show the rsync changes without uploading.
  -h, --help         Show this help text.

Environment defaults:
  APT_REPO_DIR, APT_REPO_SUITE, APT_REPO_REMOTE, APT_REPO_REMOTE_DIR,
  APT_REPO_URL.
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

validate_release_name() {
  local label="$1"
  local value="$2"

  if [[ ! "${value}" =~ ^[a-z0-9][a-z0-9.-]*$ ]]; then
    printf 'invalid %s: %s\n' "${label}" "${value}" >&2
    exit 1
  fi
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

verify_signed_release() {
  local inrelease_path="$1"
  local location="$2"
  local verification_home
  local verification_keyring

  if ! grep -Fxq -- '-----BEGIN PGP SIGNED MESSAGE-----' "${inrelease_path}" || \
    ! grep -Fxq -- '-----BEGIN PGP SIGNATURE-----' "${inrelease_path}" || \
    ! grep -Fxq -- '-----END PGP SIGNATURE-----' "${inrelease_path}"; then
    printf '%s InRelease is missing an inline OpenPGP signature: %s\n' \
      "${location}" "${inrelease_path}" >&2
    return 1
  fi

  verification_home="$(mktemp -d)"
  verification_keyring="${verification_home}/keyring.gpg"
  chmod 700 "${verification_home}"
  if ! gpg --batch --homedir "${verification_home}" --import \
    "${REPO_DIR}/ironmesh-archive-keyring.asc" >/dev/null 2>&1 || \
    ! gpg --batch --homedir "${verification_home}" --output "${verification_keyring}" \
      --export >/dev/null 2>&1; then
    rm -rf "${verification_home}"
    printf 'failed to import the archive signing key for %s verification\n' "${location}" >&2
    return 1
  fi

  if ! gpgv --keyring "${verification_keyring}" "${inrelease_path}" >/dev/null 2>&1; then
    rm -rf "${verification_home}"
    printf 'failed to verify %s InRelease with the published archive key\n' "${location}" >&2
    return 1
  fi

  rm -rf "${verification_home}"
}

shell_quote() {
  local value="$1"
  printf "'%s'" "$(printf '%s' "${value}" | sed "s/'/'\\\\''/g")"
}

add_suite() {
  local suite="$1"
  local existing

  for existing in "${SUITES[@]}"; do
    if [[ "${existing}" == "${suite}" ]]; then
      return
    fi
  done
  SUITES+=("${suite}")
}

load_matrix_suites() {
  local matrix_path="$1"
  local raw_line suite architecture package_path extra
  local line_number=0 processed_rows=0

  [[ -f "${matrix_path}" ]] || {
    printf 'server-node matrix not found: %s\n' "${matrix_path}" >&2
    exit 1
  }

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
    add_suite "${suite}"
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
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--suite requires a suite name' >&2
        exit 1
      }
      add_suite "$2"
      shift 2
      ;;
    --suite=*)
      add_suite "${1#*=}"
      shift
      ;;
    --server-node-matrix)
      [[ $# -ge 2 ]] || {
        printf '%s\n' '--server-node-matrix requires a matrix file' >&2
        exit 1
      }
      MATRIX_FILE="$2"
      shift 2
      ;;
    --server-node-matrix=*)
      MATRIX_FILE="${1#*=}"
      shift
      ;;
    --remote)
      REMOTE="$2"
      shift 2
      ;;
    --remote=*)
      REMOTE="${1#*=}"
      shift
      ;;
    --remote-dir)
      REMOTE_DIR="$2"
      shift 2
      ;;
    --remote-dir=*)
      REMOTE_DIR="${1#*=}"
      shift
      ;;
    --url)
      REMOTE_URL="$2"
      shift 2
      ;;
    --url=*)
      REMOTE_URL="${1#*=}"
      shift
      ;;
    --dry-run)
      DRY_RUN=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

require_command rsync
require_command sed
require_command ssh
require_command curl
require_command gpg
require_command gpgv
require_command grep
require_command mktemp
require_command cmp

if [[ -n "${MATRIX_FILE}" ]]; then
  load_matrix_suites "${MATRIX_FILE}"
fi
if ((${#SUITES[@]} == 0)); then
  if [[ -z "${DEFAULT_SUITE}" ]]; then
    DEFAULT_SUITE="$(native_suite)"
  fi
  add_suite "${DEFAULT_SUITE}"
fi

for suite in "${SUITES[@]}"; do
  validate_release_name 'apt suite' "${suite}"
done

if [[ ! -d "${REPO_DIR}/pool" ]]; then
  printf 'package pool not found: %s\n' "${REPO_DIR}/pool" >&2
  exit 1
fi

if [[ ! -s "${REPO_DIR}/ironmesh-archive-keyring.asc" ]]; then
  printf 'archive signing key not found: %s\n' \
    "${REPO_DIR}/ironmesh-archive-keyring.asc" >&2
  exit 1
fi

for suite in "${SUITES[@]}"; do
  if [[ ! -f "${REPO_DIR}/dists/${suite}/InRelease" ]]; then
    printf 'signed apt metadata not found: %s\n' "${REPO_DIR}/dists/${suite}/InRelease" >&2
    printf 'Run ./scripts/build-apt-repository.sh with a signing key first.\n' >&2
    exit 1
  fi
  if ! verify_signed_release "${REPO_DIR}/dists/${suite}/InRelease" "local ${suite}"; then
    exit 1
  fi
done

verify_deployed_suite() {
  local suite="$1"
  local remote_suite_dir="${REMOTE_DISTS_DIR}/${suite}"
  local remote_inrelease public_inrelease

  remote_inrelease="$(mktemp)"
  if ! ssh "${REMOTE}" "cat $(shell_quote "${remote_suite_dir}/InRelease")" \
    > "${remote_inrelease}"; then
    rm -f "${remote_inrelease}"
    printf 'failed to download deployed %s InRelease from %s for verification\n' \
      "${suite}" "${REMOTE}" >&2
    exit 1
  fi
  if ! verify_signed_release "${remote_inrelease}" "deployed remote ${suite}" || \
    ! cmp -s "${REPO_DIR}/dists/${suite}/InRelease" "${remote_inrelease}"; then
    rm -f "${remote_inrelease}"
    printf 'deployed %s InRelease differs from the locally verified metadata\n' "${suite}" >&2
    exit 1
  fi
  rm -f "${remote_inrelease}"

  public_inrelease="$(mktemp)"
  if ! curl --fail --silent --show-error --location \
    --output "${public_inrelease}" \
    "${REMOTE_URL%/}/dists/${suite}/InRelease"; then
    rm -f "${public_inrelease}"
    printf 'failed to download public %s InRelease for verification\n' "${suite}" >&2
    exit 1
  fi
  if ! verify_signed_release "${public_inrelease}" "public ${suite}" || \
    ! cmp -s "${REPO_DIR}/dists/${suite}/InRelease" "${public_inrelease}"; then
    rm -f "${public_inrelease}"
    printf 'public %s InRelease differs from the locally verified metadata\n' "${suite}" >&2
    exit 1
  fi
  rm -f "${public_inrelease}"
}

RSYNC_ARGS=(-av --delete)
RSYNC_ADDITION_ARGS=(-av)
if [[ "${DRY_RUN}" == true ]]; then
  RSYNC_ARGS+=(--dry-run)
  RSYNC_ADDITION_ARGS+=(--dry-run)
fi

REMOTE_DISTS_DIR="${REMOTE_DIR%/}/dists"

if [[ "${DRY_RUN}" == false ]]; then
  for suite in "${SUITES[@]}"; do
    remote_suite_dir="${REMOTE_DISTS_DIR}/${suite}"
    log "ensuring ${REMOTE}:${REMOTE_DIR}/dists/${suite} exists"
    ssh "${REMOTE}" \
      "mkdir -p $(shell_quote "${remote_suite_dir}") && \
        chmod a+rx $(shell_quote "${REMOTE_DISTS_DIR}") $(shell_quote "${remote_suite_dir}")"
  done
fi

log "syncing package pool additions"
rsync "${RSYNC_ADDITION_ARGS[@]}" \
  --chmod=Du=rwx,Dgo=rx,Fu=rw,Fgo=r \
  "${REPO_DIR%/}/pool/" \
  "${REMOTE}:${REMOTE_DIR%/}/pool/"

log "syncing archive signing key"
rsync "${RSYNC_ADDITION_ARGS[@]}" \
  --chmod=Du=rwx,Dgo=rx,Fu=rw,Fgo=r \
  "${REPO_DIR%/}/ironmesh-archive-keyring.asc" \
  "${REMOTE}:${REMOTE_DIR%/}/ironmesh-archive-keyring.asc"

for suite in "${SUITES[@]}"; do
  log "syncing ${suite} metadata"
  rsync "${RSYNC_ARGS[@]}" \
    --chmod=Du=rwx,Dgo=rx,Fu=rw,Fgo=r \
    "${REPO_DIR%/}/dists/${suite}/" \
    "${REMOTE}:${REMOTE_DIR%/}/dists/${suite}/"
done

if [[ "${DRY_RUN}" == true ]]; then
  log "dry run complete"
else
  for suite in "${SUITES[@]}"; do
    verify_deployed_suite "${suite}"
  done
  log "published ${REMOTE_URL%/}/"
fi
