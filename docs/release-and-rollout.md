# Release automation and home-cluster rollout

This is the shared operational reference for the `release-commit`,
`release-publish`, and `production-deploy` skills. Recheck workflow files and
live GitHub state before each release: a configured job, a successful build,
publication, and installation on a host are separate milestones.

## CI platform and delivery matrix

| Platform / role | Current CI coverage | Delivery and remaining operator work |
| --- | --- | --- |
| Linux AMD64 | Rust checks, tests, coverage, web tests, Linux system tests and QUIC network tests; static musl Server Node build and execution smoke test; client/FUSE/rendezvous binary bundle and full Ubuntu 24.04 Debian package validation | Stable tags publish the Server Node through signed APT. The full client/rendezvous package bundle remains an Actions artifact; it is not automatically promoted to APT or GitHub Releases. Host upgrades are operator-driven. |
| Linux ARM64 | Static musl Server Node cross-build, ELF/artifact verification, QEMU `--version` smoke, and distribution package wrappers | Stable tags publish the Server Node through signed APT. This does not provide a native ARM64 system-test lane or ARM64 desktop/client packages. |
| Windows x64 | CFAPI checks/tests, workspace clippy, Windows system tests, opt-in/tag-triggered unsigned MSI validation | `Release` builds and signs the Server Node MSI and client MSIX, then publishes GitHub assets. Partner Center submission and installation/testing remain operator steps; the MSI updater is described in [windows-server-node-auto-update.md](windows-server-node-auto-update.md). |
| Android | Client and Server Node JVM tests, Server Node debug APK validation, client benchmark compilation, client emulator instrumentation; signed internal release APKs when enabled | APKs are Actions artifacts. No Play Store publication or device rollout is automated. |
| iOS | Rust facade and simulator tests on a macOS runner; release archive outside ordinary PRs | Signing/export to IPA and TestFlight upload are implemented but conditional on configuration. No App Store production submission is automated. |
| macOS Server Node | Package-structure regression test inside the iOS job, using a fixture executable | The real native/universal Server Node `.pkg` build, Developer ID signing, notarization, publication, and installation remain manual. See [macos-server-node.md](macos-server-node.md). This is not a complete native macOS Server Node/system-test lane. |
| Linux ARMv7 / LuckFox | Local Cortex-A7 musl build/deploy helpers exist | No ARMv7 CI build, `armhf` APT package, or release publication lane exists. `deploy-server-node-to-luckfox.sh` builds locally and uploads a binary; it is a separate hardware deployment, not part of the three-node rollout. |

The automated **Server Node** APT matrix is exactly:

| Distribution | Suite | Architectures |
| --- | --- | --- |
| Ubuntu 20.04 | `focal` | `amd64`, `arm64` |
| Debian 13 | `trixie` | `amd64`, `arm64` |
| Ubuntu 24.04 | `noble` | `amd64` |

Do not infer support for another suite/architecture from the portable binary.
The workflows run on GitHub-hosted Linux, Windows, and macOS runners; Android
and iOS are additional application targets, not additional runner families.

## Triggers, approvals, and publication

- [check.yml](../.github/workflows/check.yml) runs on `main`, `v*` tags, PRs,
  and manual dispatch. PR labels opt into expensive package/archive work:
  `ci:debian-packages`, `ci:android-release`, `ci:ios-release`, and
  `ci:windows-server-node-msi`. Most labels affect a subset of jobs, not a
  complete release. Windows client MSIX release building/signing lives in
  `Release`, not the ordinary PR matrix.
- Android client release APKs run on non-PR CI events or labeled PRs when the
  internal signing keystore is present. Server Node release APKs run only for
  labeled PRs or stable release tags, also requiring the keystore. Without it,
  the release matrix entries are omitted. Artifact names begin with
  `android-release-performance-` and `android-server-node-release-performance-`.
  The workflow still reads the repository's `IRONMESH_ANDROID_INTERNAL_RELEASE_*`
  secrets into `BERRYKEEP_*` process variables; do not rename configured secrets
  based only on the product rename.
- iOS archives run on non-PR CI events and `ci:ios-release` PRs. Complete signing
  inputs enable IPA export; otherwise CI uploads
  `ios-release-archive-unsigned-*`. Signed exports use `ios-manual-install-*`.
  TestFlight additionally requires the upload flag, an `app-store-connect`
  (or accepted `app-store`) export method, and App Store Connect API credentials.
  The workflow currently reads `IRONMESH_IOS_*` and
  `IRONMESH_APP_STORE_CONNECT_API_*` repository settings. See the exact names in
  the `ios-build` job's `env` block in `check.yml`; a green unsigned archive
  does not prove TestFlight readiness.
- [release.yml](../.github/workflows/release.yml) starts on `v*` pushes or a
  manual dispatch with an existing tag. It requires an annotated stable
  `vX.Y.Z` tag matching the workspace version. Windows Server Node signing uses
  `release-signing`; client signing uses `windows-client-signing`. GitHub
  publication waits for **both** signing jobs. Setup details are in the
  [CI runbook](ci-runbook.md).
- [arm64-focal-packages.yml](../.github/workflows/arm64-focal-packages.yml),
  despite its historical filename, builds all five APT targets. It runs on
  `main`, stable tags, manual dispatch, and `ci:debian-packages` PRs. Stable tags
  must also be reachable from `main`. Tags automatically request signing and
  publication through `apt-repository`; ordinary `main` builds do not publish.
  Manual `publish=true` on `main` is a repair path for that branch's source,
  not a way to republish an arbitrary old tag.
- APT publication and Windows GitHub publication are independent workflows.
  One can succeed while the other fails or waits for approval. A tag push can
  therefore update APT before the GitHub Release is public. Record both runs.
  APT promotes the same verified static Server Node per CPU ABI into its suite
  wrappers, signs archive metadata, and deploys primary and compatibility
  archives. It does not install packages on cluster nodes.

The required branch checks are `Required CI`, `Required coverage`, and
`Required system tests`. `Sys` and `Cov` run on PRs and `main`, not tag pushes;
check their exact target-branch commit rather than expecting tag runs. The
aggregate checks do not cover every optional/platform/package/security job.
Inspect the requested release deliverables and security findings separately.

## Release preparation and verification

1. Refresh `origin/main` and inspect exact-commit checks, current environment
   restrictions, and signing configuration before preparing the release.
   Product/workflow fixes land separately from version metadata.
   If a cluster rollout is planned, retain the installed Server Node and
   transition packages from the trusted archive (with hashes) and secure
   configuration backups **before tag publication**. The archive builder prunes
   superseded Server Node versions and deployment deletes their pool files;
   downloading the previous version after publication may be too late.
2. Use the requested version, update the workspace version, refresh both Cargo
   lockfiles with Cargo, and add a Debian changelog entry. For an iOS release,
   also reconcile `MARKETING_VERSION` in `apps/apple-file-provider/project.yml`
   and regenerate/check the committed Xcode project as described in its README.
   CI overrides only `CURRENT_PROJECT_VERSION`; its archive check confirms
   app/extension agreement, not equality with the Cargo release version.
   Verify the archived marketing version explicitly. Deliver the change
   through a PR under the repository's normal integration rules. After squash
   integration, verify the actual final commit on `main`; do not tag the PR's
   pre-squash SHA. Wait for its required checks and relevant delivery builds.
3. Create and push the annotated stable tag only on that verified target commit.
   The tag starts the publication paths above. An authorized reviewer must
   approve any waiting protected-environment jobs. Missing credentials or
   rejected approval is a blocker, not a reason to bypass signing.
4. Verify the Windows GitHub Release contains:
   - `berrykeep-server-node-X.Y.Z-windows-x64.msi`;
   - `berrykeep-server-node-stable.json` and its `.p7s` signature;
   - `berrykeep-client-X.Y.Z.0-windows-x64.msix` and matching `.cer`;
   - `SHA256SUMS` covering all five payload/signature files.
   Check hashes, MSI Authenticode/CMS signer and manifest agreement, and the
   client MSIX signature/publisher/certificate. The client `.msixupload` is
   retained only in the `release-windows-client` Actions artifact for a later
   Partner Center upload; it is not a GitHub Release asset.
5. Independently verify APT publication, all five suite/architecture indexes,
   trusted `InRelease` signatures, and package versions/checksums. Do not use
   `--allow-unauthenticated` or replace the installed archive trust key with an
   unchecked download. See [self-hosted-apt-repository.md](self-hosted-apt-repository.md).
6. Verify tag CI artifacts for requested Android/iOS deliverables and record any
   unavailable signing/store capability. Complete the relevant manual flows in
   [ci-runbook.md](ci-runbook.md#manual-release-validation), including packaged
   Windows restart/reconnect, before signing off the release.

Before any future Android Store submission, also check monotonic package version
codes: the Server Node currently has a fixed `versionCode = 1`. Internal APK
build success does not establish store upgrade readiness.

Never move a published tag. For Windows publication repair, dispatch `Release`
from the permitted branch with the immutable `tag` input. For an old Linux
release, retain/verify that tag's package provenance instead of dispatching
`publish=true` against a newer `main` and calling it the old release.

## Home-cluster rollout

Publishing APT is automated; upgrading the LAN hosts is not. A release request
does not implicitly select every deployment target. Use the user's specified
environment and scope, carrying forward deployment authorization already given.

The established LAN rollout order is `192.168.178.92`, then
`192.168.178.110`, then `192.168.178.86`. Check each host before the next one.
Use key-only SSH. Discover its actual OS, architecture, installed packages,
active unit, config/state paths, and health endpoint before changing it.

1. Require successful publication for the intended release. On each host,
   refresh APT indexes using its configured trusted archive, inspect the exact
   suite/architecture candidate, and verify that rollback packages/configuration
   were retained before publication. If not, recover and verify prior release
   artifacts or a trusted package cache before installing; do not assume the
   pruned archive still serves the old version. The compatibility URL
   `https://creax.de/apt/ironmesh` remains supported; changing it is not required
   for an upgrade to BerryKeep.
2. Download/retain the exact intended package from the authenticated index,
   record its SHA-256, and simulate the version-pinned install. Resolve any
   removal, downgrade, held package, or unexpected configuration-path change
   before applying. Upgrade `berrykeep-server-node` and any required installed
   transition package; do not unconditionally install client and rendezvous
   packages merely because the historical helper did so. The release APT
   matrix only refreshes the Server Node and applicable compatibility packages;
   retained client/rendezvous packages may have older versions.
3. Install the pinned release using the host's existing secure sudo mechanism.
   Never put passwords in tool arguments, environment variables, or logs. Keep
   operator-maintained conffiles and the existing service identity. On legacy
   hosts, restart and validate `ironmesh-server-node`; enabling
   `berrykeep-server-node` with a new empty configuration would be a separate
   service/state migration.
4. Verify installed package version, binary `--version`, active process/unit,
   authenticated HTTPS health, and cluster/client connectivity. Record the
   target, tag/commit, package version/hash, times, and result. Stop on failure;
   restore the retained package/configuration where appropriate and verify
   recovery before proceeding. Do not remove staged or rollback artifacts
   until the authorized rollout passes.

Standalone rendezvous and telemetry are a separate, explicitly scoped part of
the rollout. Their existing cluster-`92` directories are
`creature@creax.de:/home/creature/ironmesh/clusters/92` and
`root@217.160.159.105:/root/ironmesh/clusters/92`; Strato telemetry is under
`/root/ironmesh/telemetry`. Never substitute cluster `37`. Inspect the existing
start command/PID, failover files, and secret-file permissions without printing
secrets. Preserve those directories and persistent state across the rename.

There is currently no CI-published standalone musl rendezvous or telemetry
release artifact. The helpers `scripts/deploy-rendezvous-service.sh` and
`scripts/deploy-strato-stats-collector-service.sh` build locally by default;
`--skip-build` only reuses local output, not verified release provenance. If
these services are requested, identify this gap before deployment and obtain
authorization for a clean exact-tag build (with recorded hashes), or add a
release publication lane first. The telemetry wrapper defaults to the new
`/root/berrykeep/telemetry`; the existing installation needs an explicit
`--remote-dir /root/ironmesh/telemetry` override. Do not create a second service
or empty database by silently accepting a new default.

The personal `update-home-network-nodes` skill's bundled shell script is a
legacy source-build rollout: it expects old package names, including the
removed `ironmesh-client` build output, and old rendezvous PID/binary names.
It is not the released-artifact rollout path. Use this reference and the
repository's `production-deploy` skill for current releases.

## Validated snapshot: 2026-10-18

These observations establish what has worked; recheck them before execution.

- Audited source: `main` at `7162bcd45d7305f5afa2e574b879cfb104696791`, workspace
  version `2.0.1`. Its [CI run](https://github.com/cre4ture/berrykeep/actions/runs/39040140315)
  and [five-target package build](https://github.com/cre4ture/berrykeep/actions/runs/39040140325)
  succeeded. Package publication on that `main` run was correctly skipped.
- Release `v2.0.1` has successful [Windows publication](https://github.com/cre4ture/berrykeep/actions/runs/38233107240)
  and [signed APT publication](https://github.com/cre4ture/berrykeep/actions/runs/38233107385).
  Its [GitHub Release](https://github.com/cre4ture/berrykeep/releases/tag/v2.0.1)
  contains the six files listed above; Linux/mobile assets are not attached.
- `release-signing`, `windows-client-signing`, and `apt-repository` exist, have
  required-reviewer rules, and permit `main` and stable-style tags. The configured
  APT URLs are `/apt/berrykeep` and `/apt/ironmesh` on `creax.de`.
- Android internal signing secrets are configured. iOS signing, App Store
  Connect secrets, and iOS upload/export variables are absent. The latest iOS
  run archived successfully but skipped signing, IPA export, and TestFlight.
  iOS marketing metadata still says `2.0.0`, independently of Cargo `2.0.1`;
  it needs reconciliation for an iOS `2.1.0` release.
- Read-only SSH inspection found all three LAN hosts on Ubuntu 24.04 (`noble`),
  `amd64`, with `berrykeep-server-node` and transitional `ironmesh-server-node`
  at `2.0.1-1~repo2~noble.1`, active `ironmesh-server-node`, and the compatibility
  APT URL. Their installed client and rendezvous packages remain at `1.1.0`.
  The standalone services still use the `ironmesh` directories above, with
  `berrykeep-rendezvous-service` binary/PID filenames. No deployment or release
  mutation was performed for this audit.
