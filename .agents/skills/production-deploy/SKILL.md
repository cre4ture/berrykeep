---
name: production-deploy
description: Roll out a verified BerryKeep release to production or the home cluster using signed APT packages or published release assets, with target-specific validation and rollback. Identify source-build gaps for standalone services and hardware.
---

# Production Deploy

Build once, deploy many: promote the same immutable release artifact through environments.

Read [the release and home-rollout reference](../../../docs/release-and-rollout.md),
especially its rollout procedure, observed service identities, artifact gaps,
and helper defaults. Recheck live state before acting.

1. Use a published GitHub release asset identified by immutable tag, filename,
   and SHA-256, or an exact-version APT package whose hash is authenticated by
   trusted signed archive metadata and tied to the intended release run. Linux
   APT packages are not GitHub Release attachments. Do not deploy arbitrary
   branch builds, PR artifacts, or unauthenticated packages.
2. Require the target environment and deployment authorization; carry forward
   authorization already given. A release/publication request alone does not
   select LAN hosts, standalone services, or hardware. Honor protected
   environment approval when applicable.
3. Prefer staging/canary validation before promotion. For the established home
   LAN, roll `.92`, `.110`, then `.86`, checking each before proceeding. Discover
   actual suite/architecture and pin the package version. APT publication does
   not upgrade hosts automatically.
4. Preserve operator data, configuration, and the active service identity,
   including legacy `ironmesh-*` units/paths. Retain rollback packages before
   publication prunes them, or verify retained prior-release artifacts/cache
   before installation, and inspect the simulated upgrade. Do not turn an
   upgrade into an implicit state-path migration. Verify exact targets before
   synchronization or deletion.
5. Standalone rendezvous, telemetry, and LuckFox lack CI-published release
   artifacts. If requested, make the gap explicit and require authorization for
   a clean exact-tag source build or implement publication first. The personal
   legacy home-rollout shell script is not a released-artifact installer;
   `--skip-build` in a deploy helper is not proof of provenance.
6. Record release tag/commit, package version, asset hash, target, start/end
   time, service/binary version, and health/connectivity results. Stop on a
   failed canary or unverifiable artifact/version and verify recovery before
   continuing. Retain staged/rollback artifacts until the rollout passes.
