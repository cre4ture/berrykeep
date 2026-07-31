#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PACKAGE_NAME="stats-collector-server"
BINARY_NAME="stats-collector-server"
MANIFEST_PATH="${ROOT_DIR}/crates/stats-collector-server/Cargo.toml"

TARGET_TRIPLE="${IRONMESH_STATS_COLLECTOR_DEPLOY_TARGET:-x86_64-unknown-linux-musl}"
REMOTE_DIR="${IRONMESH_STATS_COLLECTOR_DEPLOY_REMOTE_DIR:-}"
BIND_ADDR="${IRONMESH_STATS_COLLECTOR_DEPLOY_BIND_ADDR:-0.0.0.0:44044}"
TLS_CERT_PATH="${IRONMESH_STATS_COLLECTOR_DEPLOY_TLS_CERT_PATH:-}"
TLS_KEY_PATH="${IRONMESH_STATS_COLLECTOR_DEPLOY_TLS_KEY_PATH:-}"
HEALTH_URL="${IRONMESH_STATS_COLLECTOR_DEPLOY_HEALTH_URL:-}"
TLS_SERVER_NAME="${IRONMESH_STATS_COLLECTOR_DEPLOY_TLS_SERVER_NAME:-}"
STOP_TIMEOUT_SECS="${IRONMESH_STATS_COLLECTOR_DEPLOY_STOP_TIMEOUT_SECS:-20}"
AUTO_ADD_TARGET="${IRONMESH_STATS_COLLECTOR_DEPLOY_AUTO_ADD_TARGET:-true}"

SKIP_BUILD=false
DRY_RUN=false
APPLY=false
REMOTE_HOST=""
LOCAL_BINARY=""
EXPECTED_PACKAGE_VERSION=""
REMOTE_BINARY=""
REMOTE_PIDFILE=""
REMOTE_LOGFILE=""
REMOTE_ENV_FILE=""
REMOTE_START_SCRIPT=""
REMOTE_DB_PATH=""
REMOTE_HAD_BINARY=false

declare -a SSH_OPTIONS=()
declare -a SCP_OPTIONS=()

log() {
  printf '[deploy-stats-collector-service] %s\n' "$*"
}

fail() {
  printf '[deploy-stats-collector-service] error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage:
  scripts/deploy-stats-collector-service.sh [options] --apply HOST
  scripts/deploy-stats-collector-service.sh [options] --dry-run HOST

Build the standalone stats collector as a static MUSL binary with the bundled
offline country resolver, deploy it to one remote host over key-only SSH, and
verify the public HTTPS health endpoint. Existing database and environment
files are preserved. On first deployment, an admin token is generated on the
remote host without printing or transferring it.

Required:
  --remote-dir PATH       Remote service directory.
  --tls-cert-path PATH    Remote PEM certificate path.
  --tls-key-path PATH     Remote PEM private-key path.
  --health-url URL        Public HTTPS /health URL used after restart.
  --apply                 Authorize upload, replacement, and service restart.

Options:
  --bind-addr ADDR        Listener address (default: 0.0.0.0:44044).
  --tls-server-name NAME  Expected certificate hostname; defaults to the host
                          parsed from --health-url.
  --target TRIPLE         Rust target (default: x86_64-unknown-linux-musl).
  --stop-timeout SECS     Graceful shutdown timeout (default: 20).
  --skip-build            Reuse the existing target release binary.
  --ssh-option OPT        Append one ssh option token; repeat as needed.
  --scp-option OPT        Append one scp option token; repeat as needed.
  --dry-run               Print the resolved deployment plan without changes.
  -h, --help              Show this help.

The remote layout is:
  <remote-dir>/stats-collector-server
  <remote-dir>/stats-collector-server.env
  <remote-dir>/stats-collector-server.pid
  <remote-dir>/stats-collector-server.log
  <remote-dir>/start.sh
  <remote-dir>/data/stats-collector.sqlite3

The script never prints the admin token or private-key contents. If activation
or health verification fails, the previous binary is restored and restarted.
EOF
}

quote_for_sh() {
  printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

is_truthy() {
  case "${1,,}" in
    1|true|yes|y)
      return 0
      ;;
    0|false|no|n)
      return 1
      ;;
    *)
      fail "invalid boolean value: $1"
      ;;
  esac
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

parse_args() {
  while (($# > 0)); do
    case "$1" in
      --remote-dir)
        (($# >= 2)) || fail '--remote-dir requires a value'
        REMOTE_DIR="$2"
        shift 2
        ;;
      --remote-dir=*)
        REMOTE_DIR="${1#*=}"
        shift
        ;;
      --bind-addr)
        (($# >= 2)) || fail '--bind-addr requires a value'
        BIND_ADDR="$2"
        shift 2
        ;;
      --bind-addr=*)
        BIND_ADDR="${1#*=}"
        shift
        ;;
      --tls-cert-path)
        (($# >= 2)) || fail '--tls-cert-path requires a value'
        TLS_CERT_PATH="$2"
        shift 2
        ;;
      --tls-cert-path=*)
        TLS_CERT_PATH="${1#*=}"
        shift
        ;;
      --tls-key-path)
        (($# >= 2)) || fail '--tls-key-path requires a value'
        TLS_KEY_PATH="$2"
        shift 2
        ;;
      --tls-key-path=*)
        TLS_KEY_PATH="${1#*=}"
        shift
        ;;
      --health-url)
        (($# >= 2)) || fail '--health-url requires a value'
        HEALTH_URL="$2"
        shift 2
        ;;
      --health-url=*)
        HEALTH_URL="${1#*=}"
        shift
        ;;
      --tls-server-name)
        (($# >= 2)) || fail '--tls-server-name requires a value'
        TLS_SERVER_NAME="$2"
        shift 2
        ;;
      --tls-server-name=*)
        TLS_SERVER_NAME="${1#*=}"
        shift
        ;;
      --target)
        (($# >= 2)) || fail '--target requires a value'
        TARGET_TRIPLE="$2"
        shift 2
        ;;
      --target=*)
        TARGET_TRIPLE="${1#*=}"
        shift
        ;;
      --stop-timeout)
        (($# >= 2)) || fail '--stop-timeout requires a value'
        STOP_TIMEOUT_SECS="$2"
        shift 2
        ;;
      --stop-timeout=*)
        STOP_TIMEOUT_SECS="${1#*=}"
        shift
        ;;
      --ssh-option)
        (($# >= 2)) || fail '--ssh-option requires a value'
        SSH_OPTIONS+=("$2")
        shift 2
        ;;
      --ssh-option=*)
        SSH_OPTIONS+=("${1#*=}")
        shift
        ;;
      --scp-option)
        (($# >= 2)) || fail '--scp-option requires a value'
        SCP_OPTIONS+=("$2")
        shift 2
        ;;
      --scp-option=*)
        SCP_OPTIONS+=("${1#*=}")
        shift
        ;;
      --skip-build)
        SKIP_BUILD=true
        shift
        ;;
      --dry-run)
        DRY_RUN=true
        shift
        ;;
      --apply)
        APPLY=true
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      -*)
        fail "unknown option: $1"
        ;;
      *)
        [[ -z "${REMOTE_HOST}" ]] || fail 'provide exactly one remote host'
        REMOTE_HOST="$1"
        shift
        ;;
    esac
  done
}

resolve_layout() {
  [[ -n "${REMOTE_HOST}" ]] || fail 'provide exactly one remote host'
  [[ -n "${REMOTE_DIR}" ]] || fail 'set --remote-dir'
  [[ "${REMOTE_DIR}" == /* && "${REMOTE_DIR}" != "/" ]] || \
    fail '--remote-dir must be an absolute, non-root path'
  [[ -n "${TLS_CERT_PATH}" ]] || fail 'set --tls-cert-path'
  [[ -n "${TLS_KEY_PATH}" ]] || fail 'set --tls-key-path'
  [[ -n "${HEALTH_URL}" ]] || fail 'set --health-url'
  [[ "${HEALTH_URL}" == https://* ]] || fail '--health-url must use https://'
  if [[ -z "${TLS_SERVER_NAME}" ]]; then
    TLS_SERVER_NAME="${HEALTH_URL#https://}"
    TLS_SERVER_NAME="${TLS_SERVER_NAME%%/*}"
    TLS_SERVER_NAME="${TLS_SERVER_NAME%%:*}"
  fi
  [[ -n "${TLS_SERVER_NAME}" ]] || fail 'failed deriving a TLS server name'
  [[ "${STOP_TIMEOUT_SECS}" =~ ^[0-9]+$ ]] || \
    fail '--stop-timeout must be a non-negative integer'

  REMOTE_DIR="${REMOTE_DIR%/}"
  REMOTE_BINARY="${REMOTE_DIR}/${BINARY_NAME}"
  REMOTE_PIDFILE="${REMOTE_DIR}/${BINARY_NAME}.pid"
  REMOTE_LOGFILE="${REMOTE_DIR}/${BINARY_NAME}.log"
  REMOTE_ENV_FILE="${REMOTE_DIR}/${BINARY_NAME}.env"
  REMOTE_START_SCRIPT="${REMOTE_DIR}/start.sh"
  REMOTE_DB_PATH="${REMOTE_DIR}/data/stats-collector.sqlite3"
}

print_dry_run() {
  log "host: ${REMOTE_HOST}"
  log "remote directory: ${REMOTE_DIR}"
  log "binary: ${REMOTE_BINARY}"
  log "database preserved at: ${REMOTE_DB_PATH}"
  log "listener: ${BIND_ADDR} with native TLS"
  log "certificate: ${TLS_CERT_PATH}"
  log "health verification: ${HEALTH_URL}"
  log "build: ${PACKAGE_NAME} --features bundled-country-db --target ${TARGET_TRIPLE}"
  log 'plan: key-only SSH preflight, MUSL build, checksum-verified upload, restart, version/HTTPS verification, rollback on failure'
}

resolve_local_build_artifact() {
  local metadata package_pattern target_pattern target_dir

  metadata="$(cargo metadata --manifest-path "${MANIFEST_PATH}" --no-deps --format-version=1)"
  package_pattern="\"name\":\"${PACKAGE_NAME}\",\"version\":\"([^\"]+)\""
  if [[ "${metadata}" =~ ${package_pattern} ]]; then
    EXPECTED_PACKAGE_VERSION="${BASH_REMATCH[1]}"
  else
    fail "failed resolving package version for ${PACKAGE_NAME}"
  fi

  target_pattern='"target_directory":"([^"]+)"'
  if [[ "${metadata}" =~ ${target_pattern} ]]; then
    target_dir="${BASH_REMATCH[1]}"
  else
    fail 'failed resolving Cargo target directory'
  fi
  LOCAL_BINARY="${target_dir}/${TARGET_TRIPLE}/release/${BINARY_NAME}"
}

ensure_target_installed() {
  if rustup target list --installed | grep -Fxq "${TARGET_TRIPLE}"; then
    return 0
  fi
  if is_truthy "${AUTO_ADD_TARGET}"; then
    log "installing Rust target ${TARGET_TRIPLE}"
    rustup target add "${TARGET_TRIPLE}"
    return 0
  fi
  fail "Rust target ${TARGET_TRIPLE} is not installed"
}

build_binary() {
  if [[ "${SKIP_BUILD}" == true ]]; then
    [[ -x "${LOCAL_BINARY}" ]] || fail "local binary not found: ${LOCAL_BINARY}"
    log "reusing existing ${TARGET_TRIPLE} release binary"
  else
    ensure_target_installed
    log "building ${PACKAGE_NAME} ${EXPECTED_PACKAGE_VERSION} for ${TARGET_TRIPLE}"
    cargo build \
      --locked \
      --manifest-path "${MANIFEST_PATH}" \
      --release \
      --target "${TARGET_TRIPLE}" \
      --features bundled-country-db
  fi

  [[ -x "${LOCAL_BINARY}" ]] || fail "build did not produce ${LOCAL_BINARY}"
  local version_output
  version_output="$("${LOCAL_BINARY}" --version)"
  [[ "${version_output}" == "${BINARY_NAME} ${EXPECTED_PACKAGE_VERSION}" ]] || \
    fail "unexpected local binary version: ${version_output}"
  log "verified local binary version ${EXPECTED_PACKAGE_VERSION}"
}

preflight_remote() {
  local remote_dir_q cert_q key_q binary_q pidfile_q
  remote_dir_q="$(quote_for_sh "${REMOTE_DIR}")"
  cert_q="$(quote_for_sh "${TLS_CERT_PATH}")"
  key_q="$(quote_for_sh "${TLS_KEY_PATH}")"
  binary_q="$(quote_for_sh "${REMOTE_BINARY}")"
  pidfile_q="$(quote_for_sh "${REMOTE_PIDFILE}")"
  local tls_server_name_q
  tls_server_name_q="$(quote_for_sh "${TLS_SERVER_NAME}")"

  log "preflight ${REMOTE_HOST}"
  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_DIR=${remote_dir_q} TLS_CERT_PATH=${cert_q} TLS_KEY_PATH=${key_q} REMOTE_BINARY=${binary_q} REMOTE_PIDFILE=${pidfile_q} TLS_SERVER_NAME=${tls_server_name_q} bash -s" <<'REMOTE'
set -euo pipefail
test "$(uname -m)" = x86_64
test "$REMOTE_DIR" != /
test -r "$TLS_CERT_PATH"
test -r "$TLS_KEY_PATH"
command -v curl >/dev/null
command -v openssl >/dev/null
command -v sha256sum >/dev/null
openssl x509 -in "$TLS_CERT_PATH" -noout -checkhost "$TLS_SERVER_NAME" >/dev/null
cert_pub="$(openssl x509 -in "$TLS_CERT_PATH" -pubkey -noout |
  openssl pkey -pubin -outform pem 2>/dev/null |
  sha256sum | cut -d' ' -f1)"
key_pub="$(openssl pkey -in "$TLS_KEY_PATH" -pubout -outform pem 2>/dev/null |
  sha256sum | cut -d' ' -f1)"
test "$cert_pub" = "$key_pub"

if [[ -f "$REMOTE_PIDFILE" ]]; then
  pid="$(tr -d '[:space:]' <"$REMOTE_PIDFILE" || true)"
  if [[ -n "$pid" ]] && kill -0 "$pid" >/dev/null 2>&1; then
    cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline")"
    process_cwd="$(readlink "/proc/$pid/cwd")"
    [[ "$process_cwd" == "$REMOTE_DIR" ]] &&
      [[ "$cmdline" == "$REMOTE_BINARY"* || "$cmdline" == "./$(basename "$REMOTE_BINARY")"* ]] || {
      echo "pid file points to an unrelated process" >&2
      exit 1
    }
  fi
fi
printf 'host=%s architecture=x86_64 tls=valid\n' "$(hostname)"
REMOTE
}

upload_binary() {
  local checksum remote_tmp remote_tmp_q remote_dir_q remote_binary_q checksum_q
  checksum="$(sha256sum "${LOCAL_BINARY}" | cut -d' ' -f1)"
  remote_tmp="${REMOTE_BINARY}.new.$$"
  remote_tmp_q="$(quote_for_sh "${remote_tmp}")"
  remote_dir_q="$(quote_for_sh "${REMOTE_DIR}")"
  remote_binary_q="$(quote_for_sh "${REMOTE_BINARY}")"
  checksum_q="$(quote_for_sh "${checksum}")"

  log "uploading checksum-verified binary to ${REMOTE_HOST}"
  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_DIR=${remote_dir_q} bash -s" <<'REMOTE'
set -euo pipefail
install -d -m 0700 "$REMOTE_DIR"
REMOTE
  scp "${SCP_OPTIONS[@]}" "${LOCAL_BINARY}" "${REMOTE_HOST}:${remote_tmp}"
  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_TMP=${remote_tmp_q} EXPECTED_CHECKSUM=${checksum_q} REMOTE_BINARY=${remote_binary_q} bash -s" <<'REMOTE'
set -euo pipefail
actual_checksum="$(sha256sum "$REMOTE_TMP" | cut -d' ' -f1)"
test "$actual_checksum" = "$EXPECTED_CHECKSUM"
chmod 0755 "$REMOTE_TMP"
if [[ -x "$REMOTE_BINARY" ]]; then
  printf 'existing_binary=yes\n'
else
  printf 'existing_binary=no\n'
fi
REMOTE

  if ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" "test -x ${remote_binary_q}"; then
    REMOTE_HAD_BINARY=true
  fi
}

configure_remote() {
  local remote_dir_q env_q start_q db_q bind_q cert_q key_q
  remote_dir_q="$(quote_for_sh "${REMOTE_DIR}")"
  env_q="$(quote_for_sh "${REMOTE_ENV_FILE}")"
  start_q="$(quote_for_sh "${REMOTE_START_SCRIPT}")"
  db_q="$(quote_for_sh "${REMOTE_DB_PATH}")"
  bind_q="$(quote_for_sh "${BIND_ADDR}")"
  cert_q="$(quote_for_sh "${TLS_CERT_PATH}")"
  key_q="$(quote_for_sh "${TLS_KEY_PATH}")"

  log "ensuring remote configuration without exposing secrets"
  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_DIR=${remote_dir_q} ENV_FILE=${env_q} START_SCRIPT=${start_q} DB_PATH=${db_q} BIND_ADDR=${bind_q} TLS_CERT_PATH=${cert_q} TLS_KEY_PATH=${key_q} bash -s" <<'REMOTE'
set -euo pipefail
umask 077
install -d -m 0700 "$REMOTE_DIR" "$(dirname "$DB_PATH")"
touch "$ENV_FILE"
chmod 0600 "$ENV_FILE"

set_setting() {
  local name="$1"
  local value="$2"
  local tmp="${ENV_FILE}.new.$$"
  awk -v prefix="${name}=" 'index($0, prefix) != 1 { print }' "$ENV_FILE" >"$tmp"
  printf '%s=%q\n' "$name" "$value" >>"$tmp"
  chmod 0600 "$tmp"
  mv "$tmp" "$ENV_FILE"
}

set_setting STATS_COLLECTOR_BIND_ADDR "$BIND_ADDR"
set_setting STATS_COLLECTOR_DB_PATH "$DB_PATH"
set_setting STATS_COLLECTOR_K_ANONYMITY_MIN 5
set_setting STATS_COLLECTOR_RETENTION_DAYS 180
set_setting STATS_COLLECTOR_TLS_CERT_PATH "$TLS_CERT_PATH"
set_setting STATS_COLLECTOR_TLS_KEY_PATH "$TLS_KEY_PATH"
set_setting STATS_COLLECTOR_TLS_RELOAD_SECS 86400
set_setting RUST_LOG info

if ! grep -q '^STATS_COLLECTOR_ADMIN_TOKEN=' "$ENV_FILE"; then
  admin_token="$(openssl rand -hex 32)"
  printf 'STATS_COLLECTOR_ADMIN_TOKEN=%q\n' "$admin_token" >>"$ENV_FILE"
  unset admin_token
fi

start_tmp="${START_SCRIPT}.new.$$"
cat >"$start_tmp" <<'START'
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
set -a
source ./stats-collector-server.env
set +a
exec ./stats-collector-server
START
chmod 0700 "$start_tmp"
mv "$start_tmp" "$START_SCRIPT"
printf 'configuration=ready admin_token=preserved-or-generated\n'
REMOTE
}

stop_remote_service() {
  local binary_q pidfile_q remote_dir_q timeout_q
  binary_q="$(quote_for_sh "${REMOTE_BINARY}")"
  pidfile_q="$(quote_for_sh "${REMOTE_PIDFILE}")"
  remote_dir_q="$(quote_for_sh "${REMOTE_DIR}")"
  timeout_q="$(quote_for_sh "${STOP_TIMEOUT_SECS}")"

  log "stopping existing service on ${REMOTE_HOST}"
  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_BINARY=${binary_q} REMOTE_PIDFILE=${pidfile_q} REMOTE_DIR=${remote_dir_q} STOP_TIMEOUT_SECS=${timeout_q} bash -s" <<'REMOTE'
set -euo pipefail
declare -a pids=()

process_matches() {
  local pid="$1"
  local cmdline process_cwd
  [[ "$pid" =~ ^[0-9]+$ ]] || return 1
  kill -0 "$pid" >/dev/null 2>&1 || return 1
  cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline")"
  process_cwd="$(readlink "/proc/$pid/cwd")"
  [[ "$process_cwd" == "$REMOTE_DIR" ]] &&
    [[ "$cmdline" == "$REMOTE_BINARY"* || "$cmdline" == "./$(basename "$REMOTE_BINARY")"* ]]
}

if [[ -f "$REMOTE_PIDFILE" ]]; then
  pid="$(tr -d '[:space:]' <"$REMOTE_PIDFILE" || true)"
  if [[ "$pid" =~ ^[0-9]+$ ]] && kill -0 "$pid" >/dev/null 2>&1; then
    process_matches "$pid" || {
      echo "refusing to stop unrelated pid $pid" >&2
      exit 1
    }
    pids+=("$pid")
  fi
fi

for process_dir in /proc/[0-9]*; do
  pid="${process_dir##*/}"
  process_matches "$pid" || continue
  if [[ ! " ${pids[*]-} " =~ [[:space:]]${pid}[[:space:]] ]]; then
    pids+=("$pid")
  fi
done

if [[ ${#pids[@]} -eq 0 ]]; then
  rm -f "$REMOTE_PIDFILE"
  echo 'service not running'
  exit 0
fi

for pid in "${pids[@]}"; do
  kill "$pid" >/dev/null 2>&1 || true
done
deadline=$((SECONDS + STOP_TIMEOUT_SECS))
while ((SECONDS < deadline)); do
  running=false
  for pid in "${pids[@]}"; do
    if kill -0 "$pid" >/dev/null 2>&1; then
      running=true
    fi
  done
  [[ "$running" == false ]] && break
  sleep 1
done
for pid in "${pids[@]}"; do
  if kill -0 "$pid" >/dev/null 2>&1; then
    echo "service pid $pid did not stop within ${STOP_TIMEOUT_SECS}s" >&2
    exit 1
  fi
done
rm -f "$REMOTE_PIDFILE"
echo 'service stopped'
REMOTE
}

activate_uploaded_binary() {
  local binary_q remote_tmp_q
  binary_q="$(quote_for_sh "${REMOTE_BINARY}")"
  remote_tmp_q="$(quote_for_sh "${REMOTE_BINARY}.new.$$")"

  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_BINARY=${binary_q} REMOTE_TMP=${remote_tmp_q} bash -s" <<'REMOTE'
set -euo pipefail
test -x "$REMOTE_TMP"
if [[ -x "$REMOTE_BINARY" ]]; then
  cp -p "$REMOTE_BINARY" "${REMOTE_BINARY}.previous"
fi
mv "$REMOTE_TMP" "$REMOTE_BINARY"
REMOTE
}

start_remote_service() {
  local remote_dir_q pidfile_q logfile_q start_q
  remote_dir_q="$(quote_for_sh "${REMOTE_DIR}")"
  pidfile_q="$(quote_for_sh "${REMOTE_PIDFILE}")"
  logfile_q="$(quote_for_sh "${REMOTE_LOGFILE}")"
  start_q="$(quote_for_sh "${REMOTE_START_SCRIPT}")"

  log "starting updated service on ${REMOTE_HOST}"
  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_DIR=${remote_dir_q} REMOTE_PIDFILE=${pidfile_q} REMOTE_LOGFILE=${logfile_q} START_SCRIPT=${start_q} bash -s" <<'REMOTE'
set -euo pipefail
cd "$REMOTE_DIR"
rm -f "$REMOTE_PIDFILE"
nohup "$START_SCRIPT" >>"$REMOTE_LOGFILE" 2>&1 </dev/null &
pid="$!"
printf '%s\n' "$pid" >"$REMOTE_PIDFILE"
sleep 2
kill -0 "$pid"
printf 'service started pid=%s\n' "$pid"
REMOTE
}

verify_deployment() {
  local binary_q pidfile_q env_q db_q expected_q health_body
  binary_q="$(quote_for_sh "${REMOTE_BINARY}")"
  pidfile_q="$(quote_for_sh "${REMOTE_PIDFILE}")"
  env_q="$(quote_for_sh "${REMOTE_ENV_FILE}")"
  db_q="$(quote_for_sh "${REMOTE_DB_PATH}")"
  expected_q="$(quote_for_sh "${EXPECTED_PACKAGE_VERSION}")"

  log "verifying remote process, version, state, and secret permissions"
  if ! ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_BINARY=${binary_q} REMOTE_PIDFILE=${pidfile_q} ENV_FILE=${env_q} DB_PATH=${db_q} EXPECTED_VERSION=${expected_q} bash -s" <<'REMOTE'
set -euo pipefail
pid="$(tr -d '[:space:]' <"$REMOTE_PIDFILE")"
kill -0 "$pid"
test "$("$REMOTE_BINARY" --version)" = "stats-collector-server $EXPECTED_VERSION"
test -f "$DB_PATH"
test "$(stat -c '%a' "$ENV_FILE")" = 600
test "$(stat -c '%a' "$(dirname "$DB_PATH")")" = 700
printf 'pid=%s version=%s state=running\n' "$pid" "$EXPECTED_VERSION"
REMOTE
  then
    return 1
  fi

  log "verifying public TLS health endpoint ${HEALTH_URL}"
  if ! health_body="$(curl \
    --silent \
    --show-error \
    --fail \
    --connect-timeout 7 \
    --max-time 15 \
    "${HEALTH_URL}")"
  then
    return 1
  fi
  if ! grep -Fq '"status":"ok"' <<<"${health_body}"; then
    log "health response did not report ok: ${health_body}"
    return 1
  fi
  if ! grep -Fq "\"software_version\":\"${EXPECTED_PACKAGE_VERSION}\"" <<<"${health_body}"; then
    log "health response did not report version ${EXPECTED_PACKAGE_VERSION}: ${health_body}"
    return 1
  fi
  log "public HTTPS health verified for version ${EXPECTED_PACKAGE_VERSION}"
}

rollback_remote() {
  log 'activation failed; attempting rollback'
  stop_remote_service || true
  if [[ "${REMOTE_HAD_BINARY}" != true ]]; then
    log 'no previous binary exists; leaving the failed first deployment stopped'
    return 0
  fi

  local binary_q
  binary_q="$(quote_for_sh "${REMOTE_BINARY}")"
  ssh "${SSH_OPTIONS[@]}" "${REMOTE_HOST}" \
    "REMOTE_BINARY=${binary_q} bash -s" <<'REMOTE'
set -euo pipefail
test -x "${REMOTE_BINARY}.previous"
mv "${REMOTE_BINARY}.previous" "$REMOTE_BINARY"
REMOTE
  start_remote_service
  log 'previous binary restored and restarted'
}

main() {
  parse_args "$@"
  resolve_layout

  if [[ "${DRY_RUN}" == true ]]; then
    print_dry_run
    exit 0
  fi
  [[ "${APPLY}" == true ]] || \
    fail 'pass --apply to authorize deployment, or use --dry-run'

  require_command cargo
  require_command curl
  require_command rustup
  require_command scp
  require_command sed
  require_command sha256sum
  require_command ssh

  resolve_local_build_artifact
  preflight_remote
  build_binary
  upload_binary
  configure_remote
  stop_remote_service
  activate_uploaded_binary

  if ! start_remote_service || ! verify_deployment; then
    rollback_remote
    fail 'deployment failed and rollback was attempted'
  fi

  log "deployment completed: ${REMOTE_HOST}:${REMOTE_DIR} version ${EXPECTED_PACKAGE_VERSION}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
