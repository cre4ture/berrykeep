---
name: release-publish
description: Publish and verify a signed BerryKeep GitHub release from an existing annotated stable tag. Use to follow the tag-triggered release workflow, verify artifact provenance, or repair publication without creating a new product release.
---

# Release Publish

1. Start only from an existing annotated `vX.Y.Z` tag whose workspace version matches the tag. Treat the tag as immutable.
2. Treat the tag-triggered `Release` workflow as the source of truth; never publish pull-request CI artifacts.
3. Fail closed on signing. Require the protected `release-signing` environment for the Windows Server Node MSI and never attach an unsigned MSI to a public release.
4. Verify artifact provenance: the release must contain the versioned MSI, `berrykeep-server-node-stable.json`, its `.p7s` signature, and `SHA256SUMS`; manifest version, MSI SHA-256, CMS signer thumbprint, and Authenticode signer must describe the same release.
5. Make retries idempotent by updating the draft or existing release assets. Never move, delete, or retag the release tag. Report the release URL and final workflow status; escalate missing signing material, certificate failures, or tag/version mismatches instead of bypassing them.
