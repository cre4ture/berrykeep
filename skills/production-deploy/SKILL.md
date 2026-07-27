---
name: production-deploy
description: Deploy an already published and verified BerryKeep release artifact to a production target. Use when Codex must perform a deliberate server, Apt repository, or hardware deployment after release publication and needs approvals, staged validation, and rollback-aware execution.
---

# Production Deploy

1. Deploy only an asset from a published GitHub release, identified by immutable tag, filename, and SHA-256. Do not deploy a branch build or a workflow artifact.
2. Require an explicit target environment and deployment authorization. Use a protected GitHub Environment or an explicit operator confirmation for remote state changes.
3. Prefer a staging target first. Validate service health and version before promoting the same asset to production.
4. Preserve existing operator data and configuration. Do not use broad synchronization or deletion flags until the exact remote target has been verified.
5. Record release tag, asset hash, target, start/end time, and health-check result. Stop and report when the artifact, signer, or remote version cannot be verified.
