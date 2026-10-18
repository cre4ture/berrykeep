---
name: release-commit
description: Cut a BerryKeep release commit and annotated tag. Use when preparing a release by gating the exact release base on green CI, choosing a version, updating release metadata and lockfiles, and pushing the atomic release commit and tag.
---

# Release Commit

Keep the final release commit atomic: product and CI fixes land first; the release commit contains versioning and release metadata only unless the caller explicitly requests otherwise.

Read [references/berrykeep-release-facts.md](references/berrykeep-release-facts.md) for stable repository-specific facts. Derive mutable conventions from the current repository and recent release history.

Read [the release automation reference](../../../docs/release-and-rollout.md)
for the current platform matrix, tag side effects, and remaining manual work.
Recheck its dated observations against current workflows and GitHub state.

## 1. Resolve the release base

- Identify the branch that will actually receive the release commit.
- Do not release from a stale feature branch or an already merged/closed PR branch. Switch to a clean, up-to-date worktree at the real release target when needed.
- Refresh remote state before release edits.
- If the request includes a later cluster rollout, retain the currently
  installed packages and secure configuration backups before publishing the
  tag: APT publication prunes superseded Server Node packages. Use the shared
  rollout reference; a release-only request does not select deployment hosts.

## 2. Pass the release gate

- Treat remote CI on the exact release-base commit and exact target branch as the source of truth.
- Run local formatting and clippy checks before pushing.
- Repair failing CI in separate commits and push those fixes before continuing.
- Continue only when the intended release base is green remotely.

## 3. Prepare the atomic release commit

- Use the caller's target version when provided. Otherwise default to a patch bump only from a stable `x.y.z`; ask for the exact version when the channel is ambiguous or the current version is a prerelease.
- Update `[workspace.package].version` in the root `Cargo.toml`.
- Refresh the root `Cargo.lock` and `tests/system-tests/Cargo.lock` with Cargo; do not hand-edit lockfiles.
- Update `debian/changelog`, preserving the current package metadata and Debian revision style, with a concise summary since the previous release tag.
- For requested iOS deliverables, reconcile `MARKETING_VERSION` in the Apple
  project specification and generated Xcode project. Cargo version changes do
  not update it; tag CI overrides only the build number. Check the archived
  marketing version against the requested release.
- Stage only release files and follow the current release commit/tag convention discovered from recent history.
- Deliver the metadata change through the repository's PR workflow. Tag the
  final integrated commit on the release target, not the pre-squash PR head.

## 4. Tag, push, and hand off publication

- Refresh remote state again and verify CI on the exact integrated release
  commit and target branch before creating/pushing the annotated `vX.Y.Z` tag.
  The APT tag validator requires that commit to be reachable from `main`.
- A stable tag starts both Windows GitHub publication and signed Linux APT
  publication, independently, plus tag CI/mobile artifact builds. Inspect
  protected-environment requirements before pushing; APT may publish before
  the GitHub Release is public.
- Use [../release-publish/SKILL.md](../release-publish/SKILL.md) to verify both
  publication paths and requested mobile artifacts. Cluster installation is
  a separate [production deployment](../production-deploy/SKILL.md).
