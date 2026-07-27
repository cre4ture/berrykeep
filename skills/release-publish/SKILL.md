---
name: release-publish
description: Publish and verify a signed BerryKeep GitHub release after an annotated stable tag is pushed. Use when Codex must follow the tag-triggered release workflow, validate release assets and checksums, or repair a failed release publication without creating a new product release.
---

# Release Publish

1. Start only from an existing annotated `vX.Y.Z` tag whose workspace version matches the tag.
2. Treat the tag-triggered `Release` workflow as the source of truth; do not publish CI validation artifacts from a pull request.
3. Require the protected `release-signing` environment for the Windows Server Node MSI. Never attach an unsigned MSI to a public GitHub release.
4. Verify the release contains the versioned MSI, `berrykeep-server-node-stable.json`, its `.p7s` signature, and `SHA256SUMS`.
5. Check that the manifest version, MSI SHA-256, CMS signer thumbprint, and Authenticode signer describe the same release.
6. On a retry, update the draft or existing release assets idempotently. Do not move, delete, or retag the release tag.
7. Report the GitHub release URL and the final workflow status. Escalate missing signing secrets, certificate failures, or a mismatched tag/version instead of bypassing the check.
