#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_DIR="${ROOT_DIR}/.."
ARCHITECTURE="$(dpkg --print-architecture)"

usage() {
  cat <<'EOF'
Usage: scripts/test-debian-package-transition.sh [--package-dir DIR]

Verify the supported server-package transition with the supplied packages in
an isolated dpkg root. The host package database is not changed.
EOF
}

while (($# > 0)); do
  case "$1" in
    --package-dir)
      PACKAGE_DIR="$2"
      shift 2
      ;;
    --package-dir=*)
      PACKAGE_DIR="${1#*=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'unknown option: %s\n' "$1" >&2
      exit 1
      ;;
  esac
done

require_command() {
  command -v "$1" >/dev/null 2>&1 || {
    printf '%s is required but was not found in PATH\n' "$1" >&2
    exit 1
  }
}

find_package() {
  local package_name="$1"
  local -a matches=()

  mapfile -t matches < <(find "${PACKAGE_DIR}" -maxdepth 1 -type f \
    -name "${package_name}_*_${ARCHITECTURE}.deb" -print | sort)
  if ((${#matches[@]} != 1)); then
    printf 'expected one %s package for %s in %s, found %s\n' \
      "${package_name}" "${ARCHITECTURE}" "${PACKAGE_DIR}" "${#matches[@]}" >&2
    exit 1
  fi
  printf '%s\n' "${matches[0]}"
}

for command_name in dpkg dpkg-deb dpkg-query; do
  require_command "${command_name}"
done
PACKAGE_DIR="$(cd "${PACKAGE_DIR}" && pwd)"

declare -A PACKAGE_PATHS=()
for package_name in \
  ironmesh-server-node \
  ironmesh-server-node-map-tools \
  ironmesh-rendezvous-service \
  berrykeep-client \
  berrykeep-server-node \
  berrykeep-server-node-map-tools \
  berrykeep-rendezvous-service; do
  PACKAGE_PATHS["${package_name}"]="$(find_package "${package_name}")"
done

TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "${TEST_ROOT}"' EXIT
DPKG_ROOT="${TEST_ROOT}/root"
mkdir -p "${DPKG_ROOT}/var/lib/dpkg"

make_legacy_package() {
  local package_name="$1"
  local package_root="${TEST_ROOT}/legacy-${package_name}"

  mkdir -p "${package_root}/DEBIAN"
  printf 'Package: %s\nVersion: 0.0.0-1\nArchitecture: %s\nMaintainer: Test <test@example.invalid>\nDescription: pre-rename package fixture\n' \
    "${package_name}" "${ARCHITECTURE}" > "${package_root}/DEBIAN/control"

  case "${package_name}" in
    ironmesh-server-node)
      mkdir -p "${package_root}/usr/bin" "${package_root}/etc/ironmesh"
      printf 'legacy server node\n' > "${package_root}/usr/bin/ironmesh-server-node"
      chmod 0755 "${package_root}/usr/bin/ironmesh-server-node"
      printf 'IRONMESH_SERVER_BIND=127.0.0.1:8443\n' > "${package_root}/etc/ironmesh/server-node.env"
      printf '%s\n' '/etc/ironmesh/server-node.env' > "${package_root}/DEBIAN/conffiles"
      ;;
    ironmesh-rendezvous-service)
      mkdir -p "${package_root}/usr/bin" "${package_root}/etc/ironmesh"
      printf 'legacy rendezvous\n' > "${package_root}/usr/bin/ironmesh-rendezvous-service"
      chmod 0755 "${package_root}/usr/bin/ironmesh-rendezvous-service"
      printf 'IRONMESH_RENDEZVOUS_PUBLIC_URL=https://rendezvous.example\n' > "${package_root}/etc/ironmesh/rendezvous-service.env"
      printf '%s\n' '/etc/ironmesh/rendezvous-service.env' > "${package_root}/DEBIAN/conffiles"
      ;;
    ironmesh-server-node-map-tools)
      mkdir -p "${package_root}/usr/share/ironmesh-map-tools"
      printf 'legacy map tools\n' > "${package_root}/usr/share/ironmesh-map-tools/README"
      ;;
  esac

  dpkg-deb --root-owner-group --build "${package_root}" "${TEST_ROOT}/${package_name}-old.deb" >/dev/null
}

unpack_package() {
  dpkg --root="${DPKG_ROOT}" --force-not-root --force-script-chrootless --force-depends \
    --unpack "$1" >/dev/null 2>&1
}

configure_package() {
  dpkg --root="${DPKG_ROOT}" --force-not-root --force-script-chrootless --force-depends \
    --force-confold --configure "$1" >/dev/null 2>&1
}

test_conffile_pending_or_installed() {
  local path="$1"

  test -f "${path}" || test -f "${path}.dpkg-new"
}

for package_name in \
  ironmesh-server-node \
  ironmesh-server-node-map-tools \
  ironmesh-rendezvous-service; do
  make_legacy_package "${package_name}"
  unpack_package "${TEST_ROOT}/${package_name}-old.deb"
done

for package_name in \
  berrykeep-client \
  berrykeep-server-node \
  berrykeep-server-node-map-tools \
  berrykeep-rendezvous-service \
  ironmesh-server-node \
  ironmesh-server-node-map-tools \
  ironmesh-rendezvous-service; do
  unpack_package "${PACKAGE_PATHS[${package_name}]}"
done
configure_package berrykeep-client

test -x "${DPKG_ROOT}/usr/bin/berrykeep"
test -f "${DPKG_ROOT}/usr/share/applications/berrykeep-config-app.desktop"
test -f "${DPKG_ROOT}/etc/xdg/autostart/berrykeep-config-app-background.desktop"
test -f "${DPKG_ROOT}/usr/lib/berrykeep-client/gnome-shell-extension/berrykeep-status@berrykeep.io/extension.js"
test -f "${DPKG_ROOT}/usr/lib/berrykeep-client/gnome-shell-extension/berrykeep-status@berrykeep.io/icons/berrykeep-brand-symbolic.svg"
grep -Fxq "const BerryKeepIndicator = GObject.registerClass(" \
  "${DPKG_ROOT}/usr/lib/berrykeep-client/gnome-shell-extension/berrykeep-status@berrykeep.io/extension.js"
grep -Fxq '  "uuid": "berrykeep-status@berrykeep.io",' \
  "${DPKG_ROOT}/usr/lib/berrykeep-client/gnome-shell-extension/berrykeep-status@berrykeep.io/metadata.json"
test "$(readlink "${DPKG_ROOT}/usr/bin/ironmesh-server-node")" = berrykeep-server-node
test "$(readlink "${DPKG_ROOT}/usr/bin/ironmesh-rendezvous-service")" = berrykeep-rendezvous-service
# Server packages have maintainer scripts, which this isolated test intentionally
# does not run. dpkg therefore retains their replacement conffiles as .dpkg-new
# until the real apt transaction configures the packages.
test_conffile_pending_or_installed "${DPKG_ROOT}/etc/ironmesh/server-node.env"
test_conffile_pending_or_installed "${DPKG_ROOT}/etc/ironmesh/rendezvous-service.env"
test -f "${DPKG_ROOT}/usr/lib/systemd/system/ironmesh-server-node.service"
test -f "${DPKG_ROOT}/usr/lib/systemd/system/ironmesh-rendezvous-service.service"
grep -Fxq 'Conflicts=ironmesh-server-node.service' \
  "${DPKG_ROOT}/usr/lib/systemd/system/berrykeep-server-node.service"
grep -Fxq 'Conflicts=ironmesh-rendezvous-service.service' \
  "${DPKG_ROOT}/usr/lib/systemd/system/berrykeep-rendezvous-service.service"

printf 'Debian server package transition passed\n'
