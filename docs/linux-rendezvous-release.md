# Linux Rendezvous release artifacts

The `Release` workflow builds the static x86_64 Linux Rendezvous artifact through
the reusable `Linux Rendezvous release` workflow. Both the cluster-specific and
global Rendezvous modes use this executable. It includes the embedded Iroh relay
and the HTTPS/WebSocket relay tunnel.

The release contains three additional assets:

- `berrykeep-rendezvous-service-X.Y.Z-x86_64-unknown-linux-musl.tar.gz`
- the archive's `.sha256` checksum file;
- an armored `.sha256.asc` signature of that checksum file.

The archive contains the executable, an internal `SHA256SUMS`, and
`build-metadata.json`. Metadata records the release tag, full source commit,
package version, build revision, target, and executable hash. The existing
release-wide `SHA256SUMS` continues to cover the Windows assets; the Linux
archive has its own signed checksum file.
The build revision records the binary's actual `git describe` label. Another
tag on the same commit may change that label without changing the release
identity, which is checked separately using the annotated release tag, full
commit, and package version.

## Build and signing boundary

Builds check out the exact validated source commit with its annotated tag and
locked dependencies. The artifact must be a fully static x86_64 ELF without a
dynamic interpreter or shared-library dependency. The build runs `--version`
and requires its version and revision to match the checked-out source.
The build has platform-enforced `cache-mode: none` and uses no shared compiler
or Cargo caches, so manually selected source cannot write the default branch's
cache scope. See GitHub's [cache access controls](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching#controlling-cache-access-with-cache-mode).
The build job initially checks out protected `main` without persisted Git
credentials. Trusted pipeline code verifies the requested commit's tag,
workspace version, and ancestry before selecting it, then repeats source
validation immediately before building and packaging.

Pull requests build and test unsigned development artifacts. Their metadata
does not claim a release tag, and they cannot enter the publication job.
Release inputs must be clean, match the annotated stable tag, and be reachable
from `origin/main`.

The protected `apt-repository` environment supplies the existing archive key:

- `BERRYKEEP_APT_ARCHIVE_GPG_FINGERPRINT` (variable);
- `BERRYKEEP_APT_ARCHIVE_GPG_PRIVATE_KEY_B64` (secret);
- `BERRYKEEP_APT_ARCHIVE_GPG_PASSPHRASE` (secret).

The caller forwards only these two named secrets; the reusable workflow declares
both as required. It does not inherit unrelated repository/organization secrets.
The protected environment supplies the signing values to the publication job;
the fingerprint remains a public variable.
Spaced, multiline display and lowercase fingerprints are canonicalized to the
same full uppercase fingerprint before key lookup, comparison and signing.

Publication rechecks archive metadata, executable hashes, static linkage, and
file permissions without executing downloaded signing inputs. It signs the
archive checksum with the pinned archive key. Private key material is removed
after the job. Signing utilities come from protected `main`, matching the
Windows signing jobs. Environment approval remains an independent operator step.

Configure `apt-repository` with selected deployment branches/tags, allowing
branch `main` and tag pattern `v[0-9]*.[0-9]*.[0-9]*`, and a required reviewer.
Do not enable all branches. These environment policies are the external trust
boundary: a workflow's source checks cannot constrain a ref that supplies its
own workflow definition. Before approving, check the run's controller ref,
requested product tag, and validated source commit. The stable-tag allowance
supports the parent `Release` workflow; feature branches must remain excluded.

The parent `Release` publication waits for the Linux publication to succeed.
The Linux workflow creates only a draft when the release does not yet exist;
the parent publishes it after attaching its required signed Windows assets.
Existing Linux bytes must match on retry. Existing signatures must verify
against the configured fingerprint. Conflicting assets fail instead of being
overwritten.

## Repair an existing immutable release

After this workflow is merged, dispatch `Linux Rendezvous release` from
protected `main` with `tag=vX.Y.Z`. Approve its `apt-repository` job after the
build succeeds. This appends the three Linux assets to an existing release
without rebuilding its Windows assets or moving the tag.
Existing parent `Release` dispatches from a stable tag also remain supported:
the controller tag must itself be annotated, match its workspace version, and
be reachable from `main`. Arbitrary feature-branch dispatches are rejected.

```bash
gh workflow run rendezvous-release.yml --ref main -f tag=v2.1.0
```

Build utilities come from the workflow commit, publication utilities come from
protected `main`, and all product source and the Rust toolchain version come
from the requested release commit. Toolchain channels are checked against a
single-token Rustup allowlist before becoming job outputs or action inputs.
This allows the first publication to repair tags that predate the packaging
scripts.

## Verify before deployment

Download the three exact-version assets from the published GitHub release.
Use an independently trusted archive keyring, such as the existing node's
`/usr/share/keyrings/ironmesh-archive-keyring.gpg`; do not establish trust by
downloading an unverified key alongside the archive.

```bash
asset=berrykeep-rendezvous-service-2.1.0-x86_64-unknown-linux-musl.tar.gz
gpgv --keyring /usr/share/keyrings/ironmesh-archive-keyring.gpg \
  "$asset.sha256.asc" "$asset.sha256"
sha256sum --check "$asset.sha256"
python3 scripts/rendezvous_release.py verify \
  --archive "$asset" --tag v2.1.0 \
  --commit "$(git rev-parse 'v2.1.0^{commit}')"
```

Promote the same verified executable to each authorized host. Preserve the
existing service mode, startup arguments, environment, TLS credentials,
failover material, registry, and admin token. Back up the old binary and state
before restarting one host at a time. Verify the running executable hash and
version, HTTPS health, relay probes, and authenticated discovery before
advancing to the other host. Do not use PR artifacts for a production update.
