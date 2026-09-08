#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_DIR="${ROOT_DIR}/.."
ARCHITECTURE="$(dpkg --print-architecture)"

usage() {
  cat <<'EOF'
Usage: scripts/test-debian-package-transition.sh [--package-dir DIR]

Unpack synthetic pre-rename Ironmesh and released BerryKeep package sets with
the supplied packages into isolated dpkg roots. The host package database is
not changed.
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
  berrykeep-client \
  berrykeep-server-node \
  berrykeep-server-node-map-tools \
  berrykeep-rendezvous-service \
  ironmesh-client \
  ironmesh-server-node \
  ironmesh-server-node-map-tools \
  ironmesh-rendezvous-service; do
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
    ironmesh-client)
      mkdir -p \
        "${package_root}/usr/bin" \
        "${package_root}/usr/lib/ironmesh-client/gnome-shell-extension/ironmesh-status@ironmesh.io" \
        "${package_root}/usr/share/applications" \
        "${package_root}/etc/xdg/autostart"
      for binary_name in ironmesh ironmesh-config-app; do
        printf 'legacy %s\n' "${binary_name}" > "${package_root}/usr/bin/${binary_name}"
        chmod 0755 "${package_root}/usr/bin/${binary_name}"
        printf 'legacy %s\n' "${binary_name}" > "${package_root}/usr/lib/ironmesh-client/${binary_name}"
        chmod 0755 "${package_root}/usr/lib/ironmesh-client/${binary_name}"
      done
      printf 'legacy extension\n' > "${package_root}/usr/lib/ironmesh-client/gnome-shell-extension/ironmesh-status@ironmesh.io/extension.js"
      printf 'legacy desktop entry\n' > "${package_root}/usr/share/applications/ironmesh-config-app.desktop"
      printf 'legacy autostart entry\n' > "${package_root}/etc/xdg/autostart/ironmesh-config-app-background.desktop"
      printf '%s\n' '/etc/xdg/autostart/ironmesh-config-app-background.desktop' > "${package_root}/DEBIAN/conffiles"
      ;;
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

make_released_berrykeep_client_package() {
  local package_root="${TEST_ROOT}/released-berrykeep-client"

  mkdir -p \
    "${package_root}/DEBIAN" \
    "${package_root}/usr/share/applications" \
    "${package_root}/etc/xdg/autostart"
  printf 'Package: berrykeep-client\nVersion: 1.1.0-2~repo1~test\nArchitecture: %s\nMaintainer: Test <test@example.invalid>\nBreaks: ironmesh-client (<< 1.1.0-2~repo1~test)\nReplaces: ironmesh-client (<< 1.1.0-2~repo1~test)\nDescription: released BerryKeep client fixture\n' \
    "${ARCHITECTURE}" > "${package_root}/DEBIAN/control"
  printf 'legacy desktop entry\n' > "${package_root}/usr/share/applications/ironmesh-config-app.desktop"
  printf 'legacy autostart entry\n' > "${package_root}/etc/xdg/autostart/ironmesh-config-app-background.desktop"
  printf '%s\n' '/etc/xdg/autostart/ironmesh-config-app-background.desktop' > "${package_root}/DEBIAN/conffiles"
  dpkg-deb --root-owner-group --build "${package_root}" "${TEST_ROOT}/berrykeep-client-released.deb" >/dev/null
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
  ironmesh-client \
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
  ironmesh-client \
  ironmesh-server-node \
  ironmesh-server-node-map-tools \
  ironmesh-rendezvous-service; do
  unpack_package "${PACKAGE_PATHS[${package_name}]}"
done
configure_package berrykeep-client
configure_package ironmesh-client

test -x "${DPKG_ROOT}/usr/bin/berrykeep"
test -f "${DPKG_ROOT}/usr/share/applications/berrykeep-config-app.desktop"
test -f "${DPKG_ROOT}/etc/xdg/autostart/berrykeep-config-app-background.desktop"
test "$(readlink "${DPKG_ROOT}/usr/bin/ironmesh")" = ../lib/berrykeep-client/ironmesh
test -d "${DPKG_ROOT}/usr/lib/ironmesh-client"
test "$(readlink "${DPKG_ROOT}/usr/lib/ironmesh-client/ironmesh-config-app")" = ../berrykeep-client/berrykeep-config-app
test -f "${DPKG_ROOT}/usr/lib/ironmesh-client/gnome-shell-extension/ironmesh-status@ironmesh.io/extension.js"
test -f "${DPKG_ROOT}/usr/share/applications/ironmesh-config-app.desktop"
test -f "${DPKG_ROOT}/etc/xdg/autostart/ironmesh-config-app-background.desktop"
grep -Fxq 'NoDisplay=true' "${DPKG_ROOT}/usr/share/applications/ironmesh-config-app.desktop"
grep -Fxq 'Hidden=true' "${DPKG_ROOT}/etc/xdg/autostart/ironmesh-config-app-background.desktop"
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

# The originally released berrykeep-client briefly owned the legacy desktop
# paths. Verify the new transitional package can claim those paths regardless
# of unpack order, while dpkg still retains administrator conffile changes.
DPKG_ROOT="${TEST_ROOT}/released-berrykeep-root"
mkdir -p "${DPKG_ROOT}/var/lib/dpkg"
make_released_berrykeep_client_package
unpack_package "${TEST_ROOT}/berrykeep-client-released.deb"
printf '%s\n' '# administrator setting' >> \
  "${DPKG_ROOT}/etc/xdg/autostart/ironmesh-config-app-background.desktop"
unpack_package "${PACKAGE_PATHS[ironmesh-client]}"
unpack_package "${PACKAGE_PATHS[berrykeep-client]}"
configure_package berrykeep-client
configure_package ironmesh-client
test -f "${DPKG_ROOT}/usr/share/applications/berrykeep-config-app.desktop"
test -f "${DPKG_ROOT}/etc/xdg/autostart/berrykeep-config-app-background.desktop"
grep -Fxq '# administrator setting' \
  "${DPKG_ROOT}/etc/xdg/autostart/ironmesh-config-app-background.desktop"
dpkg-query --admindir "${DPKG_ROOT}/var/lib/dpkg" --showformat='${Conffiles}\n' --show ironmesh-client | \
  grep -Fq '/etc/xdg/autostart/ironmesh-config-app-background.desktop'

printf 'Debian Ironmesh-to-BerryKeep package transition passed\n'
