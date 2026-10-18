---
name: release-publish
description: Verify or repair BerryKeep Windows GitHub and Linux APT release publication from an existing annotated stable tag, and check requested mobile CI artifacts without creating a new product release.
---

# Release Publish

Read [the release automation reference](../../../docs/release-and-rollout.md)
for the platform/artifact matrix, protected environments, evidence, and manual
store/hardware work. Recheck current workflow and GitHub configuration.

1. Start from an existing annotated `vX.Y.Z` tag whose workspace version matches
   the tag. Treat it as immutable. APT additionally requires reachability from
   `main`; use the exact tag commit for provenance.
2. Follow both `Release` and `Server Node Debian packages`. They publish
   independently; neither alone proves the full release is published. Never
   promote PR artifacts. Inspect tag `CI` for requested Android/iOS outputs.
3. Require `release-signing` for the Windows Server Node MSI,
   `windows-client-signing` for the client MSIX, and `apt-repository` for Linux
   publication. Honor existing deployment authorization and environment review
   rules; do not bypass missing credentials or approvals.
4. Verify the versioned MSI, stable JSON manifest, detached CMS signature,
   versioned client MSIX and public certificate, and `SHA256SUMS`. Check hashes,
   manifest version, MSI Authenticode/CMS signer agreement, and the client MSIX
   signature/publisher/certificate. The `.msixupload` remains an Actions artifact
   for manual Partner Center submission.
5. Verify signed APT metadata and expected Server Node versions/checksums in
   every configured suite/architecture, on primary and compatibility archives.
   APT packages are published in the archive, not attached to GitHub Releases.
6. Repair Windows publication by dispatching `Release` with the existing tag
   from a permitted branch; asset updates are idempotent. The Debian workflow's
   manual `publish=true` builds the selected `main`, not an arbitrary old tag.
   Do not use it to relabel newer binaries as an old release. Never move,
   delete, or retag the release tag.
7. Report the GitHub Release URL, both publication run results, APT version
   availability, and requested mobile artifact/signing status. Android/iOS
   Actions artifacts, TestFlight upload, store submission, and cluster rollout
   are separate outcomes. Escalate actual publication blockers rather than
   declaring the release complete after only the Windows workflow succeeds.
