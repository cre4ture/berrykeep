---
name: production-deploy
description: Deploy an already published and verified IronMesh release artifact to a production target. For the fixed home-network rollout, always deploy and verify the public Fleet Reliability dashboard with the telemetry collector.
---

# Production Deploy

1. Deploy only an asset from a published GitHub release, identified by immutable tag, filename, and SHA-256. Do not deploy a branch build or a workflow artifact.
   - Exception: when the user explicitly authorizes deployment of the current remote `origin/main` to the fixed home-network targets, fetch `origin/main`, record its immutable commit, and use a separate clean detached worktree at that commit. Do not deploy a local branch or a worktree with uncommitted changes.
2. Require an explicit target environment and deployment authorization. Use a protected GitHub Environment or an explicit operator confirmation for remote state changes.
3. Prefer a staging target first. Validate service health and version before promoting the same asset to production.
4. Preserve existing operator data and configuration. Do not use broad synchronization or deletion flags until the exact remote target has been verified.
5. Record release tag or authorized `origin/main` commit, artifact hash or package manifest hash, target, start/end time, and health-check result. Stop and report when the artifact, signer, or remote version cannot be verified.

## Fixed home-network rollout

For the fixed home-network target set, use the `update-home-network-nodes` workflow from a
clean worktree. A normal rollout must include the Strato telemetry collector; never pass
`--skip-rendezvous` unless the user has explicitly authorized a LAN-only rollout.

Before starting its package build, confirm that the operator can access the local terminal that
will receive the one shared sudo-password prompt. Do not start the rollout from a client that
cannot expose that terminal (for example, a mobile client without terminal input). Never ask for
the password in chat or place it in a command, environment variable, file, or log. In that case,
stop before building and have the operator resume from an accessible Codex desktop or CLI terminal.

The telemetry deployment is mandatory on every full home-network rollout:

1. Run the fixed rollout with `--repo <clean-origin-main-worktree> --apply`. It deploys the
   collector through `scripts/deploy-strato-stats-collector-service.sh`, which builds
   `@ironmesh/fleet-telemetry` and uploads its `dist` directory together with the collector
   binary.
2. Preserve `/root/ironmesh/telemetry/data/stats-collector.sqlite3` and the existing remote
   admin token. Never print the token, a private key, or the rendezvous passphrase.
3. Treat telemetry deployment as successful only after all of these checks pass on
   `https://217.160.159.105:9444`:
   - `/health` reports the deployed collector version;
   - `/` contains `<title>IronMesh Fleet Reliability</title>`;
   - `/v1/stats/dashboard` returns HTTP 200 with a dashboard JSON document.
4. If any telemetry build, upload, restart, health, HTML, or dashboard-API verification fails,
   stop the rollout and report the failure. Do not claim a full home-network deployment succeeded.
