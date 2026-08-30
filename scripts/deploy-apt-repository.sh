#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

REPO_DIR="${APT_REPO_DIR:-${ROOT_DIR}/target/apt-repo}"
SUITE="${APT_REPO_SUITE:-noble}"
REMOTE="${APT_REPO_REMOTE:-creature@creax.de}"
REMOTE_DIR="${APT_REPO_REMOTE_DIR:-/home/creature/html/apt/ironmesh}"
REMOTE_URL="${APT_REPO_URL:-https://creax.de/apt/ironmesh}"
DRY_RUN=false

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
  --suite NAME       Apt suite to deploy. Defaults to noble. Use trixie for
                     packages built natively on Debian Trixie.
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

validate_release_name 'apt suite' "${SUITE}"

if [[ ! -f "${REPO_DIR}/dists/${SUITE}/InRelease" ]]; then
  printf 'signed apt metadata not found: %s\n' "${REPO_DIR}/dists/${SUITE}/InRelease" >&2
  printf 'Run ./scripts/build-apt-repository.sh with a signing key first.\n' >&2
  exit 1
fi

if [[ ! -d "${REPO_DIR}/pool" ]]; then
  printf 'package pool not found: %s\n' "${REPO_DIR}/pool" >&2
  exit 1
fi

if [[ ! -s "${REPO_DIR}/ironmesh-archive-keyring.asc" ]]; then
  printf 'archive signing key not found: %s\n' \
    "${REPO_DIR}/ironmesh-archive-keyring.asc" >&2
  exit 1
fi

if ! verify_signed_release "${REPO_DIR}/dists/${SUITE}/InRelease" 'local'; then
  exit 1
fi

RSYNC_ARGS=(-av --delete)
RSYNC_ADDITION_ARGS=(-av)
if [[ "${DRY_RUN}" == true ]]; then
  RSYNC_ARGS+=(--dry-run)
  RSYNC_ADDITION_ARGS+=(--dry-run)
fi

REMOTE_DISTS_DIR="${REMOTE_DIR%/}/dists"
REMOTE_SUITE_DIR="${REMOTE_DISTS_DIR}/${SUITE}"

if [[ "${DRY_RUN}" == false ]]; then

  log "ensuring ${REMOTE}:${REMOTE_DIR}/dists/${SUITE} exists"
  ssh "${REMOTE}" \
    "mkdir -p $(shell_quote "${REMOTE_SUITE_DIR}") && \
      chmod a+rx $(shell_quote "${REMOTE_DISTS_DIR}") $(shell_quote "${REMOTE_SUITE_DIR}")"
fi

log "syncing ${SUITE} metadata"
rsync "${RSYNC_ARGS[@]}" \
  --chmod=Du=rwx,Dgo=rx,Fu=rw,Fgo=r \
  "${REPO_DIR%/}/dists/${SUITE}/" \
  "${REMOTE}:${REMOTE_DIR%/}/dists/${SUITE}/"

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

if [[ "${DRY_RUN}" == true ]]; then
  log "dry run complete"
else
  REMOTE_INRELEASE="$(mktemp)"
  if ! ssh "${REMOTE}" "cat $(shell_quote "${REMOTE_SUITE_DIR}/InRelease")" \
    > "${REMOTE_INRELEASE}"; then
    rm -f "${REMOTE_INRELEASE}"
    printf 'failed to download deployed %s InRelease from %s for verification\n' \
      "${SUITE}" "${REMOTE}" >&2
    exit 1
  fi

  if ! verify_signed_release "${REMOTE_INRELEASE}" 'deployed remote'; then
    rm -f "${REMOTE_INRELEASE}"
    exit 1
  fi
  if ! cmp -s "${REPO_DIR}/dists/${SUITE}/InRelease" "${REMOTE_INRELEASE}"; then
    rm -f "${REMOTE_INRELEASE}"
    printf 'deployed %s InRelease differs from the locally verified metadata\n' "${SUITE}" >&2
    exit 1
  fi
  rm -f "${REMOTE_INRELEASE}"

  PUBLIC_INRELEASE="$(mktemp)"
  if ! curl --fail --silent --show-error --location \
    --output "${PUBLIC_INRELEASE}" \
    "${REMOTE_URL%/}/dists/${SUITE}/InRelease"; then
    rm -f "${PUBLIC_INRELEASE}"
    printf 'failed to download public %s InRelease for verification\n' "${SUITE}" >&2
    exit 1
  fi

  if ! verify_signed_release "${PUBLIC_INRELEASE}" 'public'; then
    rm -f "${PUBLIC_INRELEASE}"
    exit 1
  fi
  if ! cmp -s "${REPO_DIR}/dists/${SUITE}/InRelease" "${PUBLIC_INRELEASE}"; then
    rm -f "${PUBLIC_INRELEASE}"
    printf 'public %s InRelease differs from the locally verified metadata\n' "${SUITE}" >&2
    exit 1
  fi
  rm -f "${PUBLIC_INRELEASE}"

  log "published ${REMOTE_URL%/}/"
fi
