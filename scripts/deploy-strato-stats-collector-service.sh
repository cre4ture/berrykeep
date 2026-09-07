#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

exec "${ROOT_DIR}/scripts/deploy-stats-collector-service.sh" \
  --remote-dir /root/ironmesh/telemetry \
  --bind-addr 0.0.0.0:9444 \
  --tls-cert-path /etc/letsencrypt/live/217.160.159.105/fullchain.pem \
  --tls-key-path /etc/letsencrypt/live/217.160.159.105/privkey.pem \
  --health-url https://217.160.159.105:9444/health \
  --dashboard-url https://217.160.159.105:9444/ \
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
  root@217.160.159.105
