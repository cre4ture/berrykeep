#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CHECK_ONLY=false
PRINT_ONLY=false
TARGET_SUITE=""

log() {
  printf '[sync-debian-version] %s\n' "$*"
}

usage() {
  cat <<'EOF'
Align debian/changelog's upstream version with Cargo.toml's workspace version.

Usage:
  ./scripts/sync-debian-version.sh [--suite SUITE] [--check] [--print]

Options:
  --suite SUITE
            Set the changelog distribution and add a suite-specific Debian
            revision suffix. This keeps packages built for different suites
            distinct, even when their upstream version is identical.
  --check   Fail if debian/changelog is not aligned; do not edit.
  --print   Print the desired Debian package version; do not edit.
  -h, --help
            Show this help text.

The script reads [workspace.package] version from Cargo.toml and converts Cargo
pre-release syntax to Debian pre-release syntax:

  1.0.0-beta.1 -> 1.0.0~beta.1

Without --suite, it preserves the Debian revision suffix from the current top
changelog entry, for example:

  1.0.0~beta.1-1~repo1~ubuntu24.04.1 -> 1.0.4-1~repo1~ubuntu24.04.1

With --suite, the distribution-specific suffix is replaced by ~SUITE.N. A
legacy ~repoN~ubuntu/debian suffix is migrated to ~repo(N+1)~SUITE.N so the
first suite-specific package sorts above an already published legacy package.
For example, a focal package becomes 1.0.4-1~repo2~focal.1 and a trixie
package becomes 1.0.4-1~repo2~trixie.1. Other revision formats receive a
+SUITE.N suffix, which also sorts above the original revision.
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

workspace_cargo_version() {
  awk '
    /^\[workspace\.package\]$/ {
      in_workspace_package = 1
      next
    }
    /^\[/ {
      in_workspace_package = 0
    }
    in_workspace_package && $1 == "version" {
      sub(/^[^"]*"/, "")
      sub(/".*$/, "")
      print
      exit
    }
  ' "${ROOT_DIR}/Cargo.toml"
}

cargo_version_to_debian_upstream() {
  local version="$1"
  printf '%s\n' "${version//-/~}"
}

validate_suite_name() {
  local suite="$1"

  if [[ ! "${suite}" =~ ^[a-z0-9][a-z0-9.-]*$ ]]; then
    printf 'invalid apt suite: %s\n' "${suite}" >&2
    exit 1
  fi
}

debian_revision_for_suite() {
  local revision="$1"
  local base_revision repository_generation current_suite current_build

  # A legacy package with a distribution suffix must be superseded by the
  # first suite-specific wrapper. Increment its repository revision once so
  # e.g. ~repo2~focal.1 sorts above ~repo1~ubuntu24.04.1.
  if [[ "${revision}" =~ ^(.+~repo)([0-9]+)~(ubuntu[0-9]+\.[0-9]+|debian[0-9]+)\.([0-9]+)$ ]]; then
    base_revision="${BASH_REMATCH[1]}"
    repository_generation="$((10#${BASH_REMATCH[2]} + 1))"
    printf '%s%s~%s.1\n' "${base_revision}" "${repository_generation}" "${TARGET_SUITE}"
    return
  fi

  # Keep the repository revision and per-suite build number after the initial
  # migration. Do not truncate an arbitrary Debian revision such as
  # "1~repo1" that has no managed distribution suffix.
  if [[ "${revision}" =~ ^(.+~repo)([0-9]+)~([a-z0-9][a-z0-9.-]*)\.([0-9]+)$ ]]; then
    base_revision="${BASH_REMATCH[1]}"
    repository_generation="${BASH_REMATCH[2]}"
    current_suite="${BASH_REMATCH[3]}"
    current_build="${BASH_REMATCH[4]}"
    if [[ "${current_suite}" == "${TARGET_SUITE}" ]]; then
      printf '%s%s~%s.%s\n' "${base_revision}" "${repository_generation}" \
        "${TARGET_SUITE}" "${current_build}"
      return
    fi
    printf '%s%s~%s.1\n' "${base_revision}" "${repository_generation}" "${TARGET_SUITE}"
    return
  fi

  # A tilde sorts before the end of a Debian version, so appending
  # "~suite.1" here would turn an otherwise valid revision into a downgrade.
  # A plus suffix sorts after the input and remains distinguishable per suite.
  if [[ "${revision}" =~ ^(.+)\+([a-z0-9][a-z0-9.-]*)\.([0-9]+)$ ]]; then
    base_revision="${BASH_REMATCH[1]}"
    current_suite="${BASH_REMATCH[2]}"
    current_build="${BASH_REMATCH[3]}"
    if [[ "${current_suite}" == "${TARGET_SUITE}" ]]; then
      printf '%s+%s.%s\n' "${base_revision}" "${TARGET_SUITE}" "${current_build}"
      return
    fi
    printf '%s+%s.1\n' "${base_revision}" "${TARGET_SUITE}"
    return
  fi

  printf '%s+%s.1\n' "${revision}" "${TARGET_SUITE}"
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
    --check)
      CHECK_ONLY=true
      shift
      ;;
    --print)
      PRINT_ONLY=true
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

require_command awk
require_command dpkg-parsechangelog
require_command sed

if [[ -n "${TARGET_SUITE}" ]]; then
  validate_suite_name "${TARGET_SUITE}"
fi

CARGO_VERSION="$(workspace_cargo_version)"
if [[ -z "${CARGO_VERSION}" ]]; then
  printf 'failed to read [workspace.package] version from Cargo.toml\n' >&2
  exit 1
fi

CURRENT_VERSION="$(cd "${ROOT_DIR}" && dpkg-parsechangelog -SVersion)"
CURRENT_SOURCE="$(cd "${ROOT_DIR}" && dpkg-parsechangelog -SSource)"
CURRENT_DISTRIBUTION="$(cd "${ROOT_DIR}" && dpkg-parsechangelog -SDistribution)"
DEBIAN_REVISION=""
if [[ "${CURRENT_VERSION}" == *-* ]]; then
  DEBIAN_REVISION="-${CURRENT_VERSION#*-}"
fi

if [[ -n "${TARGET_SUITE}" && -n "${DEBIAN_REVISION}" ]]; then
  DEBIAN_REVISION="-$(debian_revision_for_suite "${DEBIAN_REVISION#-}")"
fi

DESIRED_VERSION="$(cargo_version_to_debian_upstream "${CARGO_VERSION}")${DEBIAN_REVISION}"
DESIRED_DISTRIBUTION="${TARGET_SUITE:-${CURRENT_DISTRIBUTION}}"

if [[ "${PRINT_ONLY}" == true ]]; then
  printf '%s\n' "${DESIRED_VERSION}"
  exit 0
fi

if [[ "${CURRENT_VERSION}" == "${DESIRED_VERSION}" && \
  "${CURRENT_DISTRIBUTION}" == "${DESIRED_DISTRIBUTION}" ]]; then
  log "debian/changelog already matches Cargo version ${CARGO_VERSION} (${DESIRED_VERSION}, ${DESIRED_DISTRIBUTION})"
  exit 0
fi

if [[ "${CHECK_ONLY}" == true ]]; then
  printf 'debian/changelog has version %s and distribution %s; expected version %s and distribution %s for Cargo version %s\n' \
    "${CURRENT_VERSION}" "${CURRENT_DISTRIBUTION}" "${DESIRED_VERSION}" \
    "${DESIRED_DISTRIBUTION}" "${CARGO_VERSION}" >&2
  exit 1
fi

# Preserve every field after the changelog header's semicolon (for example
# "binary-only=yes") while updating only source/version/distribution fields.
sed -i "1s|^[^[:space:]]\\+[[:space:]]\\+(.*)[[:space:]]\\+[^;]*;|${CURRENT_SOURCE} (${DESIRED_VERSION}) ${DESIRED_DISTRIBUTION};|" \
  "${ROOT_DIR}/debian/changelog"
log "updated debian/changelog from ${CURRENT_VERSION} (${CURRENT_DISTRIBUTION}) to ${DESIRED_VERSION} (${DESIRED_DISTRIBUTION})"
