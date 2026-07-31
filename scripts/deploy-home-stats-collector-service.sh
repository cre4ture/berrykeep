#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

exec "${ROOT_DIR}/scripts/deploy-stats-collector-service.sh" \
  --remote-dir /home/creature/ironmesh/telemetry \
  --tls-cert-path /home/creature/etc/certificates/creax.de.crt \
  --tls-key-path /home/creature/etc/certificates/creax.de.key \
  --health-url https://creax.de:44044/health \
  --ssh-option=-o \
  --ssh-option=BatchMode=yes \
  --ssh-option=-o \
  --ssh-option=StrictHostKeyChecking=accept-new \
  --ssh-option=-o \
  --ssh-option=ConnectTimeout=7 \
  --ssh-option=-o \
  --ssh-option=PreferredAuthentications=publickey \
  --ssh-option=-o \
  --ssh-option=PasswordAuthentication=no \
  --ssh-option=-o \
  --ssh-option=KbdInteractiveAuthentication=no \
  --scp-option=-o \
  --scp-option=BatchMode=yes \
  --scp-option=-o \
  --scp-option=StrictHostKeyChecking=accept-new \
  --scp-option=-o \
  --scp-option=ConnectTimeout=7 \
  --scp-option=-o \
  --scp-option=PreferredAuthentications=publickey \
  --scp-option=-o \
  --scp-option=PasswordAuthentication=no \
  --scp-option=-o \
  --scp-option=KbdInteractiveAuthentication=no \
  "$@" \
  creature@creax.de
