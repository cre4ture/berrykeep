---
name: production-deploy
description: Deploy an already published and verified BerryKeep release artifact to production. Use for deliberate server, Apt repository, or hardware deployments that require authorization, staged validation, and rollback-aware execution.
---

# Production Deploy

Build once, deploy many: promote the same immutable release artifact through environments.

1. Deploy only a published GitHub release asset identified by immutable tag, filename, and SHA-256; never deploy a branch build or workflow artifact.
2. Require an explicit target environment and deployment authorization, such as a protected GitHub Environment or operator confirmation.
3. Prefer staging first. Validate service health and version, then promote the same asset to production.
4. Preserve operator data and configuration. Verify the exact remote target before using broad synchronization or deletion flags.
5. Record release tag, asset hash, target, start/end time, and health-check result. Fail closed when the artifact, signer, or deployed version cannot be verified.
