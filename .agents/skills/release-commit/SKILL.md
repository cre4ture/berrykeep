---
name: release-commit
description: Cut a BerryKeep release commit and annotated tag. Use when preparing a release by gating the exact release base on green CI, choosing a version, updating release metadata and lockfiles, and pushing the atomic release commit and tag.
---

# Release Commit

Keep the final release commit atomic: product and CI fixes land first; the release commit contains versioning and release metadata only unless the caller explicitly requests otherwise.

Read [references/ironmesh-release-facts.md](references/ironmesh-release-facts.md) for stable repository-specific facts. Derive mutable conventions from the current repository and recent release history.

## 1. Resolve the release base

- Identify the branch that will actually receive the release commit.
- Do not release from a stale feature branch or an already merged/closed PR branch. Switch to a clean, up-to-date worktree at the real release target when needed.
- Refresh remote state before release edits.

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
- Stage only release files and follow the current release commit/tag convention discovered from recent history.

## 4. Tag, push, and hand off publication

- Create the annotated `vX.Y.Z` tag, push the release commit and tag, and confirm the tag points to the intended commit.
- Verify GitHub Actions on the pushed release commit.
- Then use [../release-publish/SKILL.md](../release-publish/SKILL.md) to verify the signed release publication before declaring the release complete.
