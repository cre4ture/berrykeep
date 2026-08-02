# CI runbook

## Signed Windows Server Node releases

Pushing an annotated stable `vX.Y.Z` tag whose value matches
`[workspace.package].version` starts the `Release` workflow. It builds the
Windows Server Node MSI again from the tagged source, signs and timestamps it
inside the protected `release-signing` environment, then publishes the MSI,
signed stable manifest, and `SHA256SUMS` on the matching GitHub Release page.

Configure these protected-environment values before tagging:

- secret `BERRYKEEP_WINDOWS_SIGNING_CERTIFICATE_B64` - base64-encoded PFX;
- secret `BERRYKEEP_WINDOWS_SIGNING_CERTIFICATE_PASSWORD` - PFX password;
- variable `BERRYKEEP_WINDOWS_TIMESTAMP_URL` - approved RFC-3161 endpoint.

The normal `Win Server MSI` CI job remains intentionally unsigned and uploads
only a short-lived validation artifact. Never publish that artifact.

The validation job does not run on ordinary pushes or manual CI runs. Add the
`ci:windows-server-node-msi` label to a pull request to request it, or push a
stable `vX.Y.Z` release tag to run it automatically alongside the signed
release workflow.

## Android release builds on pull requests

Pull requests run the Android debug checks by default. To request the signed
internal release APKs, add the `ci:android-release` label to the pull request.
The label triggers a workflow run immediately and remains effective for later
pushes to the pull request while it is present.

The Android Server Node release APK is intentionally excluded from ordinary
`main` pushes and manual CI runs. It is built only for a pull request bearing
that label or for an explicit stable `vX.Y.Z` release tag.

This requires the repository secrets `IRONMESH_ANDROID_INTERNAL_RELEASE_STORE_B64`
and the corresponding release-signing credentials. For pull requests from
forks, GitHub does not expose these secrets to the standard `pull_request`
workflow, so the release legs remain unavailable there by design.

## iOS release archive builds on pull requests

Pull requests run the `ios-build` simulator test suite by default but skip the
`Release` archive, signing/export, and artifact-upload steps. To request them,
add the `ci:ios-release` label to the pull request. The label triggers a
workflow run immediately and remains effective for later pushes to the pull
request while it is present.

Ordinary `main` pushes, tag pushes, and manual `workflow_dispatch` runs always
build the `Release` archive, matching the Android release-variant behavior.
The `ios-build` job still reports `success` on a pull request without the
label: the skipped steps are not failures, so the `Required CI` aggregate
check (which depends on `ios-build`) is unaffected.

## Required checks (branch protection alignment)

For branch `main`, require these stable aggregate status checks:

- `Required CI`
- `Required coverage`
- `Required system tests`

The aggregate jobs are the branch-protection contract. `Required CI` covers
`workspace-check`, `rustfmt`, `clippy`, `unit-tests`, and `ios-build`;
`Required coverage` covers the coverage lane; and `Required system tests`
covers every operating-system entry in the system-test matrix plus the
Linux-only QUIC network-namespace test. The aggregate jobs always report a
terminal result and accept an intentionally skipped dependency. This keeps
branch protection stable when a future impact gate skips a lane that cannot
be affected by the pull request.

Do not require the implementation job names directly. Matrix expansion and
future job-level impact conditions may change those names or skip those jobs,
while the three aggregate names above remain stable. `Bazel unit` remains
advisory until the native Bazel suite has parity with the required Cargo unit
test lane and its workflow reports a result for every pull request.

Optional (recommended separately):

- `cargo-audit`
- `cargo-deny`

Why: CI lanes are intentionally split between stable and nightly. Requiring
exactly these aggregate checks prevents accidental bypass (missing nightly
lane) or false blocking (obsolete, matrix-expanded, or intentionally skipped
implementation checks).

## Pull request cache policy

Pull request jobs restore the shared Rust caches but do not write them:

- sccache keeps `SCCACHE_GHA_ENABLED=true` and sets
  `SCCACHE_GHA_RW_MODE=READ_ONLY` for pull requests, so compiler artifacts
  already cached on `main` remain available;
- every `Swatinem/rust-cache` path, including the shared Android composite
  action, uses `save-if: ${{ github.event_name != 'pull_request' }}`.
- the Focal ARM64 package build restores its mounted on-disk sccache with
  `actions/cache/restore`, then runs `actions/cache/save` only for trusted
  non-pull-request events.

GitHub scopes pull request cache writes to the pull request merge ref. Those
entries are not reusable by `main` or unrelated pull requests, so allowing
them to accumulate only increases cache churn and eviction pressure. Trusted
non-pull-request runs keep write access: pushes to `main`, scheduled runs,
manual dispatches, and release workflows continue to seed reusable caches.

## Local required-check reproduction

From the repo root, the closest local equivalent to the required branch-protection set is:

```bash
just ci-required
```

That expands to these exact required-check reproductions:

```bash
cargo fmt --all -- --check
cargo +stable check --workspace
cargo +stable clippy --workspace --all-targets -- -D warnings
cargo +stable test --workspace
cargo +stable llvm-cov \
	-p client-sdk \
	-p sync-core \
	-p transport-sdk \
	-p rendezvous-server \
	-p server-node-sdk \
	--lib \
	--all-features \
	--summary-only \
	--ignore-filename-regex 'crates/common/src/lib.rs|crates/client-sdk/src/content_addressed_client_cache.rs|crates/server-node-sdk/src/(embedded_rendezvous|setup|ui\.rs)|crates/server-node-sdk/src/web_maps(\.rs|/)' \
	--fail-under-lines 68
cd web && pnpm test:e2e:client-ui
cd web && pnpm test:e2e:server-admin
cd web && pnpm test:e2e:server-admin-rust
cd web && pnpm test:e2e:server-admin-setup-rust
cargo +nightly -Z bindeps test --manifest-path tests/system-tests/Cargo.toml --lib
# Linux only:
cargo +nightly -Z bindeps test --manifest-path tests/system-tests/Cargo.toml \
	--test quic_network -- --test-threads=1 --nocapture
```

Pass or fail rule:

- Each command must exit `0`.
- `coverage` must stay at or above the `--fail-under-lines 68` floor.
- `unit-tests` already excludes `tests/system-tests` implicitly because the workspace root excludes that crate; nightly system coverage belongs only to the `system-tests` lane.
- On Linux, `unit-tests` now also covers the packaged config-app handoff regression through `apps/config-app/tests/package_handoff.rs`, because that integration test is part of the normal `cargo test --workspace` run on `ubuntu-latest`.
- The `ios-build` lane is macOS-only. Reproduce it locally with:

```bash
just ci-ios
```

- On macOS, `just ci-required-macos` reproduces the full required set including the iOS lane.

## iOS CI artifacts

The `ios-build` lane runs on `macos-latest` and covers:

- `cargo test -p ios-app`
- `swift test` in `apps/apple-file-provider`
- `xcodebuild test` for the `IronmeshIosProject` scheme on a dynamically selected iPhone simulator, with an explicit boot-and-wait step to avoid flaky first-launch failures on macOS runners
- a `Release` archive for `IronmeshIosApp`, on pushes/tags/manual runs or on a
  pull request labeled `ci:ios-release` (see
  [iOS release archive builds on pull requests](#ios-release-archive-builds-on-pull-requests))

The `IronmeshIosProject` XCTest bundle is intentionally unhosted: it links only the shared Apple modules and no longer depends on launching `IronmeshIosApp` in the simulator.

Artifact behavior:

- When Apple signing secrets are not configured, CI uploads an unsigned `Release` `.xcarchive` for inspection.
- When Apple signing secrets are configured, CI also exports a downloadable `.ipa` for manual device installation with `Release` performance characteristics.

Configure these repository secrets for signed iOS artifacts:

- `IRONMESH_IOS_SIGNING_CERT_B64` — base64-encoded `.p12` signing certificate
- `IRONMESH_IOS_SIGNING_CERT_PASSWORD` — password for that `.p12`
- `IRONMESH_IOS_APP_PROFILE_B64` — base64-encoded provisioning profile for `dev.ironmesh.apple.iosapp`
- `IRONMESH_IOS_EXTENSION_PROFILE_B64` — base64-encoded provisioning profile for `dev.ironmesh.apple.iosapp.fileprovider`

Both provisioning profiles must grant the App Group `group.dev.ironmesh.apple.shared`
and the resolved Keychain Sharing group
`<AppIdentifierPrefix>dev.ironmesh.apple.shared-keychain`. The source entitlement uses
`$(AppIdentifierPrefix)`; Xcode expands that build-setting placeholder to the signing
team/app-identifier prefix in the built plist and entitlement. Regenerate the profiles
after enabling either capability. Any future signed macOS host and extension profiles
must grant the same pair of shared-access entitlements.

Integration note: PR #93 changes the final iOS File Provider bundle identifier from
`dev.ironmesh.apple.iosfileprovider` to `dev.ironmesh.apple.iosapp.fileprovider` and
overlaps `project.yml`, the generated `project.pbxproj`, and this signing setup. After
both changes land, replace the extension profile with one for the nested PR #93 bundle
identifier that grants both shared-access capabilities above. Whichever PR lands second
must reconcile the generated project and preserve `IronmeshSharedAccess.entitlements`
for both the iOS host and extension configurations.

Optional repository variable:

- `IRONMESH_IOS_EXPORT_METHOD` — defaults to `development`; set to `ad-hoc` when you want shareable sideload builds for registered devices.

Useful per-lane shortcuts:

- `just ci-stable`
- `just coverage`
- `just ci-web-smoke`
- `just test-system-nightly`
- `just test-quic-network` (Linux only)

## GitHub Actions cache scope

Pull-request workflows restore `Swatinem/rust-cache` entries from the default
branch but do not publish new archive entries into the pull request's isolated
merge-ref scope. Trusted `push`, `schedule`, and `workflow_dispatch` runs still
refresh those archives. This prevents short-lived pull-request archives from
evicting reusable default-branch cache entries.

The `Cache cleanup` workflow runs when a pull request closes and repeatedly
deletes batches of the largest entries under its exact
`refs/pull/<number>/merge` scope. The cleanup is intentionally capped at 800
entries: the built-in `GITHUB_TOKEN` has a 1,000-request-per-hour REST API
limit, and each cache needs an individual delete request. Deletes are spaced
one second apart to stay below GitHub's secondary mutation limits. The final
log reports any entries left after the bounded cleanup.

This reclaims entries written by legacy or misconfigured cache clients while
leaving API capacity for other workflows. Pull-request cache writers should
still use restore-only mode so that the bounded cleanup normally removes every
remaining entry. The cleanup job never checks out or executes pull-request code
and has only `actions: write` plus `contents: read` permission.

## Nightly lane fails, stable lanes pass

This usually means the failure is isolated to `tests/system-tests` and/or nightly `bindeps` behavior.

### 1) Reproduce locally with the same command

```bash
cargo +nightly -Z bindeps test --manifest-path tests/system-tests/Cargo.toml --lib
```

For a single test:

```bash
cargo +nightly -Z bindeps test --manifest-path tests/system-tests/Cargo.toml --lib -- tests::<name> --exact --nocapture
```

For the Linux-only NAT, firewall, and QUIC route tests:

```bash
sudo apt-get install iproute2 nftables
sysctl kernel.unprivileged_userns_clone
sysctl kernel.apparmor_restrict_unprivileged_userns  # Ubuntu 24.04+
just test-quic-network
```

`kernel.unprivileged_userns_clone` must be `1`. On Ubuntu 24.04 and newer,
`kernel.apparmor_restrict_unprivileged_userns` must be `0`. The test uses
Patchbay network namespaces and therefore requires `nft`, `tc`, and
unprivileged user namespaces; it does not require root at runtime.

The serial test suite starts the real Rendezvous service, Server Node, and
IronMesh Client CLI in separate Patchbay network namespaces. It covers:

- IPv4 EIM/APDF (`Nat::Home`) on both peers, without an additional firewall,
  and requires Iroh to migrate the pooled Direct QUIC connection from relay to
  a direct hole-punched path;
- the Patchbay `Hotel` profile (symmetric NAT and UDP blocked), with the Iroh
  relay enabled, and requires a relay-assisted Direct QUIC path;
- the same blocked-UDP profile with the Iroh relay disabled, and requires the
  IronMesh relay tunnel to remain usable;
- a fault endpoint placed before a healthy rendezvous endpoint. The client
  must race both Iroh relay-ticket requests and establish relay-assisted Direct
  QUIC through the healthy endpoint without exhausting the three-second budget.

The CLI exercises the same client SDK and managed routing implementation used
by app shells, so no phone simulator is needed for these transport assertions.
Client-SDK tests separately hold every configured Iroh ticket endpoint open
beyond the three-second budget and verify that Direct-only continuation or
failure remains bounded. The blocked-UDP scenario without Iroh relay separately
verifies the IronMesh relay fallback.
The Home-NAT case also runs the Rendezvous relay's UDP QUIC Address Discovery
(QAD) endpoint. Without QAD, peers behind separate NATs cannot learn their
public UDP mappings and Iroh correctly remains on its packet-forwarding relay.

### 2) Verify stable lanes are still healthy

```bash
cargo +stable check --workspace
cargo +stable clippy --workspace --all-targets -- -D warnings
cargo +stable test --workspace
```

### 3) Common root causes

- Nightly toolchain drift (new nightly behavior/regression).
- `bindeps`/artifact path changes.
- Timing-sensitive system test behavior.
- Environment assumptions in system tests (ports, startup timing).
- Missing Linux network-namespace prerequisites (`nft`, `tc`, or user namespaces).

### 4) Fast mitigation options

- Temporarily pin nightly to the last known good date in `rust-toolchain.toml`.
- Re-run failed system test with `--nocapture` and increase polling retries if truly timing-related.
- Keep stable lanes unchanged while fixing only the nightly/system-test path.

### 5) If branch protection blocks urgent merge

- Do **not** remove required checks globally.
- Use a small targeted fix PR for nightly lane only.
- Revert only the offending nightly/system-test change if needed, then follow up with a proper fix.

## Advisory security checks

`cargo-audit` and `cargo-deny` are currently advisory-only for branch protection.

Run them before a release candidate or tag cut:

```bash
cargo audit
cargo deny --exclude system-tests check advisories licenses sources bans
```

Why advisory-only today:

- the protected merge contract is the six required checks above,
- the security workflow runs on push, pull request, schedule, and manual dispatch,
- `deny.toml` already contains one explicit advisory ignore for the optional Turso path, so release sign-off still needs human triage rather than blind pass or fail.

Release rule:

- New `cargo-audit` or `cargo-deny` findings require an explicit release decision in the checklist.
- Existing ignored findings must stay documented in `deny.toml` with a concrete rationale.

## Manual release validation

These are the minimum manual flows that should be exercised before a release candidate is signed off.

### 1. Local 4-node cluster smoke

```bash
scripts/local-cluster.sh start
scripts/local-cluster.sh status
curl --fail --silent --show-error --cacert data/local-cluster/tls/ca.pem https://127.0.0.1:18080/health >/dev/null
scripts/local-cluster.sh stop
```

Pass or fail rule:

- `status` shows all expected nodes as running.
- the HTTPS `/health` check succeeds with the generated local CA.
- `stop` leaves no stray node processes behind.

### 2. Direct client enroll and CRUD smoke

Issue a bootstrap from the local-cluster helper, enroll once, then round-trip one object:

```bash
scripts/local-cluster.sh start
scripts/local-cluster.sh bootstrap manual-cli 600 1 /tmp/ironmesh-client-bootstrap.json
cargo run -p cli-client -- \
	--bootstrap-file /tmp/ironmesh-client-bootstrap.json \
	--client-identity-file /tmp/ironmesh-client-bootstrap.client-identity.json \
	enroll \
	--label manual-cli
cargo run -p cli-client -- \
	--bootstrap-file /tmp/ironmesh-client-bootstrap.json \
	--client-identity-file /tmp/ironmesh-client-bootstrap.client-identity.json \
	put notes/manual.txt "hello manual release"
cargo run -p cli-client -- \
	--bootstrap-file /tmp/ironmesh-client-bootstrap.json \
	--client-identity-file /tmp/ironmesh-client-bootstrap.client-identity.json \
	get notes/manual.txt
scripts/local-cluster.sh stop
```

Pass or fail rule:

- `enroll` writes the client identity file successfully.
- `get notes/manual.txt` returns `hello manual release`.

### 3. Embedded rendezvous relay client path

Run the guide in [docs/manual-rendezvous-relay-test.md](manual-rendezvous-relay-test.md).

Pass or fail rule:

- both nodes complete zero-touch setup,
- one client identity enrolls successfully,
- the relay-forced bootstrap on node B can read the object written through node A.

### 4. Linux FUSE live mount

Reuse the bootstrap issued in the direct-enroll flow:

```bash
mkdir -p /tmp/ironmesh-mount
cargo run -p os-integration -- \
	--bootstrap-file /tmp/ironmesh-client-bootstrap.json \
	--mountpoint /tmp/ironmesh-mount
```

In another shell, verify one existing object is visible and one new write round-trips:

```bash
cat /tmp/ironmesh-mount/notes/manual.txt
printf 'hello from fuse\n' >/tmp/ironmesh-mount/notes/fuse.txt
cargo run -p cli-client -- \
	--bootstrap-file /tmp/ironmesh-client-bootstrap.json \
	--client-identity-file /tmp/ironmesh-client-bootstrap.client-identity.json \
	get notes/fuse.txt
```

Pass or fail rule:

- the mount comes up without authentication errors,
- `/tmp/ironmesh-mount/notes/manual.txt` is readable,
- the CLI read-back returns `hello from fuse`.

### 5. Folder-agent restart or resume

```bash
mkdir -p /tmp/ironmesh-folder-agent-root
cargo run -p ironmesh-folder-agent -- \
	--root-dir /tmp/ironmesh-folder-agent-root \
	--bootstrap-file /tmp/ironmesh-client-bootstrap.json \
	--client-identity-file /tmp/ironmesh-client-bootstrap.client-identity.json \
	--remote-refresh-interval-ms 500 \
	--local-scan-interval-ms 500
```

Stop the agent, make one remote change with the CLI and one local change in the root directory, then restart the same command.

Pass or fail rule:

- a remote file changed while the agent is stopped appears locally after restart,
- a local file created while the agent is stopped uploads after restart,
- no manual state cleanup is needed between runs.

### 6. Packaged Windows sync-root restart

Run the guide in [docs/manual-windows-sync-root-restart-test.md](manual-windows-sync-root-restart-test.md).

Release rule:

- Record one successful packaged Windows run that proves sync-root reconnection after restart or update before release sign-off.
