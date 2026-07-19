#!/usr/bin/env bash
set -euo pipefail

: "${RUNNER_REPO_URL:?RUNNER_REPO_URL must be set, e.g. https://github.com/owner/repo}"
: "${RUNNER_TOKEN:?RUNNER_TOKEN must be set to a transient registration token}"

RUNNER_NAME="${RUNNER_NAME:-docker-ephemeral-$(hostname)}"
RUNNER_LABELS="${RUNNER_LABELS:-self-hosted,docker,linux}"
RUNNER_WORKDIR="${RUNNER_WORKDIR:-_work}"

cd /home/runner/actions-runner

./config.sh \
    --unattended \
    --url "$RUNNER_REPO_URL" \
    --token "$RUNNER_TOKEN" \
    --name "$RUNNER_NAME" \
    --labels "$RUNNER_LABELS" \
    --work "$RUNNER_WORKDIR" \
    --ephemeral \
    --replace

# Ephemeral runners de-register themselves after run.sh picks up and finishes one job.
# This trap only catches the case where run.sh exits/crashes before that happens, so a
# stale offline runner entry doesn't linger in the repo's runner list.
cleanup() {
    ./config.sh remove --unattended --token "$RUNNER_TOKEN" || true
}
trap cleanup EXIT

./run.sh
