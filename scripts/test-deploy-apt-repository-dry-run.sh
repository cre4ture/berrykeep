#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEST_DIR="$(mktemp -d)"
REPO_DIR="${TEST_DIR}/repository"
MOCK_BIN_DIR="${TEST_DIR}/bin"
trap 'rm -rf "${TEST_DIR}"' EXIT

mkdir -p "${REPO_DIR}/pool" "${REPO_DIR}/dists/test" "${MOCK_BIN_DIR}"
printf 'test archive key\n' > "${REPO_DIR}/berrykeep-archive-keyring.asc"
printf '%s\n' \
  '-----BEGIN PGP SIGNED MESSAGE-----' \
  '-----BEGIN PGP SIGNATURE-----' \
  '-----END PGP SIGNATURE-----' \
  > "${REPO_DIR}/dists/test/InRelease"

for command_name in ssh curl gpg gpgv; do
  printf '#!/usr/bin/env bash\nexit 0\n' > "${MOCK_BIN_DIR}/${command_name}"
  chmod +x "${MOCK_BIN_DIR}/${command_name}"
done
printf '%s\n' \
  '#!/usr/bin/env bash' \
  'set -euo pipefail' \
  'for argument in "$@"; do' \
  '  [[ "${argument}" == -* || "${argument}" == *:* ]] && continue' \
  '  [[ -e "${argument}" ]] || exit 23' \
  'done' \
  > "${MOCK_BIN_DIR}/rsync"
chmod +x "${MOCK_BIN_DIR}/rsync"

output="$(PATH="${MOCK_BIN_DIR}:${PATH}" \
  "${ROOT_DIR}/scripts/deploy-apt-repository.sh" \
  --repo-dir "${REPO_DIR}" \
  --suite test \
  --remote example.invalid \
  --dry-run 2>&1)"

if [[ -e "${REPO_DIR}/ironmesh-archive-keyring.asc" ]]; then
  printf '%s\n' 'dry-run deployment unexpectedly created the legacy key file' >&2
  exit 1
fi
grep -Fq 'would prepare the legacy archive signing-key filename' <<<"${output}"
grep -Fq 'dry run complete' <<<"${output}"

printf 'APT deployment dry-run check passed\n'
