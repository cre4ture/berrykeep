# Self-Hosted Apt Repository

Ironmesh can be published from any static HTTPS web root as a small signed apt
repository. The web server only hosts files; apt verifies the signed `Release`
metadata and the package checksums inside that metadata.

The default Ironmesh target is:

```bash
https://creax.de/apt/ironmesh
```

## Build packages

The Server Node is built once per CPU ABI as a fully static musl executable.
The current package jobs reuse `x86_64-generic` for `amd64` and
`aarch64-generic` for `arm64`, so the Server Node itself is no longer rebuilt
against each distribution's libc. The client and rendezvous binaries remain
distribution-specific and continue to use their normal native package path.

For headless non-PC targets, use the `server-node-only` build profile. It
creates only `ironmesh-server-node`, excluding the desktop client, rendezvous
service, and optional map-tools package. The profile requires a verified static
Server Node artifact and performs neither Rust compilation nor target-binary
execution; an x86 builder can therefore create an ARM64 `.deb` wrapper inside
the target suite's container.

Build the static AArch64 artifact on x86 (or download the matching artifact
from the same CI workflow run):

```bash
server_target=aarch64-unknown-linux-musl
CARGO_TARGET_DIR="$PWD/target" ./scripts/build-static-server-node.sh \
  --target "$server_target" \
  --variant-id aarch64-generic \
  --run-smoke never
```

The static builder writes a `.tar.gz`, its `.sha256` checksum, and commit
metadata below `target/static-server-node/`. Pass the tarball, not an extracted
binary, to the package helper. This verifies the archive checksum, binary
checksum, clean source revision, package version, Rust target, ELF machine,
and static-link contract before package assembly.

```bash
# Run in a Focal or Trixie container whose suite matches --suite.
./scripts/build-local-debs.sh \
  --suite trixie \
  --arch arm64 \
  --server-node-only \
  --static-server-node-artifact target/static-server-node/ironmesh-server-node-*-aarch64-generic.tar.gz \
  -- -j1
```

The package is written to the checkout's parent directory. It is safe to pass
the CI artifact into a different suite's package container only when that
container checks out the exact Git revision named by the artifact metadata.
See [Portable Static Server-Node Package Strategy](portable-server-node-package-strategy.md)
for the full artifact contract.

## Build repository metadata

Generate `pool/`, `dists/`, `Packages.gz`, `Release`, `InRelease`, and the
public archive key. Import the published repository before adding a new target;
this retains the existing suites and architectures in the deployment staging
directory.

```bash
export GPG_TTY="$(tty)"
APT_REPO_SIGN_KEY=5D7762BDB9A2A564D500DE702A2E3C589C188616 \
  ./scripts/build-apt-repository.sh \
    --import-remote creature@creax.de:/home/creature/html/apt/ironmesh
```

The repository is created under `target/apt-repo` by default. If GPG needs the
key passphrase, run the command from a normal terminal so `gpg-agent` can ask
for it.

## Publish to creax.de

Upload the generated repository to the dedicated web directory:

```bash
./scripts/deploy-apt-repository.sh
```

The default deploy target is:

```bash
creature@creax.de:/home/creature/html/apt/ironmesh
```

The deploy script replaces metadata only for the suite being published and
adds package files to a suite-specific pool. It deliberately preserves other
suites and package files, so adding Trixie/ARM64 cannot remove the existing
Focal/ARM64 or Noble/AMD64 publication. Legacy packages remain in the former
shared pool until every published suite has been refreshed; this prevents a
migration from breaking an existing suite's package index.
When the repository builder prunes a superseded package, deployment mirrors
that deletion only in the selected suite-specific pool; other suites and the
legacy shared pool remain untouched. It publishes the signed metadata before
applying those scoped deletions, so a failed metadata upload leaves extra files
rather than an index that references a missing package.

## Sign and deploy a Server Node matrix

After building one server-only package per suite and architecture, create a
matrix file. Paths may be relative to the matrix file and must not contain
spaces:

```text
# suite  architecture  package path
focal  arm64  packages/focal/ironmesh-server-node_1.1.0-1~repo2~focal.1_arm64.deb
trixie arm64  packages/trixie/ironmesh-server-node_1.1.0-1~repo2~trixie.1_arm64.deb
focal  amd64  packages/focal/ironmesh-server-node_1.1.0-1~repo2~focal.1_amd64.deb
trixie amd64  packages/trixie/ironmesh-server-node_1.1.0-1~repo2~trixie.1_amd64.deb
noble  amd64  packages/noble/ironmesh-server-node_1.1.0-1~repo2~noble.1_amd64.deb
```

The same matrix drives signing and deployment. The repository builder imports
the remote repository once, adds each suite-specific package pool and index,
and signs every `Release`/`InRelease` pair. Deployment uploads the package pool
first, then every suite's signed metadata, and verifies the uploaded and public
`InRelease` byte-for-byte against the local copy.

`build-local-debs.sh --suite` gives each suite a distinct Debian revision
suffix (for example `~repo2~focal.1` and `~repo2~trixie.1`) and writes that
suite into the package changelog distribution. The first suite-specific
package increments the legacy repository revision, so it supersedes an
otherwise identical `~repo1~ubuntu…` package in APT's version comparison.

When a server-only matrix refreshes an already-published suite, its existing
client, rendezvous, and map-tools `.deb` files are retained from the legacy
shared pool and copied into that suite's pool before the index is regenerated.
The script migrates only files explicitly listed in that suite's existing
`Packages` index. If it finds a legacy Map Tools package with an exact Server
Node dependency, it publishes a higher-versioned compatibility rebuild with
the same payload and an upstream-version minimum dependency. That lets APT
upgrade the existing Map Tools installation and the new Server Node together,
instead of holding back the Server Node or removing Map Tools. Future full
package builds use the same minimum dependency directly. This preserves desktop
packages in their original suite without allowing Focal packages to appear in
Trixie on later publishes.

```bash
export GPG_TTY="$(tty)"
APT_REPO_SIGN_KEY=5D7762BDB9A2A564D500DE702A2E3C589C188616 \
  ./scripts/build-apt-repository.sh \
    --server-node-matrix server-node-debian-matrix.txt \
    --import-remote creature@creax.de:/home/creature/html/apt/ironmesh

./scripts/deploy-apt-repository.sh \
  --server-node-matrix server-node-debian-matrix.txt
```

## CI build, signing, and deployment

`Server Node Debian packages` runs the `focal/arm64`, `trixie/arm64`,
`focal/amd64`, `trixie/amd64`, and `noble/amd64` matrix entirely on x86
GitHub-hosted runners. It cross-builds the static AArch64 binary once with Zig
and builds the static AMD64 binary once with the same verified artifact
pipeline. Each suite container verifies its matching artifact metadata and
uploads the resulting server-only package. Pull requests receive no deployment
credentials and must opt in with the `ci:debian-packages` label.

To enable the manual `publish` workflow-dispatch input, create a protected
GitHub environment named `apt-repository` and configure its required review
policy. In that environment, add these secrets:

- `IRONMESH_APT_ARCHIVE_GPG_PRIVATE_KEY_B64`: base64-encoded exported private
  archive signing key.
- `IRONMESH_APT_ARCHIVE_GPG_PASSPHRASE`: passphrase for that key.
- `IRONMESH_APT_REPOSITORY_SSH_PRIVATE_KEY`: deploy key for the static web host.
- `IRONMESH_APT_REPOSITORY_KNOWN_HOSTS`: pinned `known_hosts` entry for the
  deploy host.

Add these environment variables:

- `IRONMESH_APT_ARCHIVE_GPG_FINGERPRINT`: expected full signing-key fingerprint.
- `IRONMESH_APT_REPOSITORY_REMOTE`: SSH target, such as `creature@creax.de`.
- `IRONMESH_APT_REPOSITORY_REMOTE_DIR`: remote repository directory.
- `IRONMESH_APT_REPOSITORY_URL`: public HTTPS repository URL.

The publish job imports the private key into a fresh temporary keyring,
confirms its fingerprint, and runs the same matrix signing and deployment
scripts shown above. It is restricted to a manual run from `main` and the
protected environment; it never exposes signing or SSH secrets to a pull
request.

## Verify the published repository

After publishing, check that the signed metadata and package index are visible:

```bash
curl -fsSL https://creax.de/apt/ironmesh/dists/noble/InRelease | gpg --verify
curl -fsSL https://creax.de/apt/ironmesh/dists/noble/main/binary-amd64/Packages.gz \
  | gzip -dc \
  | grep '^Package: '
curl -fsSL https://creax.de/apt/ironmesh/dists/focal/InRelease | gpg --verify
curl -fsSL https://creax.de/apt/ironmesh/dists/focal/main/binary-arm64/Packages.gz \
  | gzip -dc \
  | grep '^Package: '
```

## Client setup

Install the repository key into apt's keyring directory:

```bash
curl -fsSL https://creax.de/apt/ironmesh/ironmesh-archive-keyring.asc \
  | sudo gpg --dearmor -o /usr/share/keyrings/ironmesh-archive-keyring.gpg
```

Add exactly one apt source, matching the distribution suite and architecture of
the host:

```bash
# Ubuntu 20.04 ARM64
echo 'deb [arch=arm64 signed-by=/usr/share/keyrings/ironmesh-archive-keyring.gpg] https://creax.de/apt/ironmesh focal main' \
  | sudo tee /etc/apt/sources.list.d/ironmesh.list
```

```bash
# Ubuntu 24.04 AMD64
echo 'deb [arch=amd64 signed-by=/usr/share/keyrings/ironmesh-archive-keyring.gpg] https://creax.de/apt/ironmesh noble main' \
  | sudo tee /etc/apt/sources.list.d/ironmesh.list
```

```bash
# Debian Trixie ARM64 / Raspberry Pi OS based on Trixie
echo 'deb [arch=arm64 signed-by=/usr/share/keyrings/ironmesh-archive-keyring.gpg] https://creax.de/apt/ironmesh trixie main' \
  | sudo tee /etc/apt/sources.list.d/ironmesh.list
```

Install or update packages through apt:

```bash
sudo apt update
sudo apt install ironmesh-client
```

Headless targets install only `ironmesh-server-node`. The desktop package set
can additionally install `ironmesh-rendezvous-service`; add
`ironmesh-server-node-map-tools` when the optional Natural Earth imports are
needed.

## Publishing updates

For a new release, bump `[workspace.package].version` in `Cargo.toml`, build the
local `.deb` packages, rebuild the repository metadata, and deploy again. The
packaging helpers update the upstream portion of `debian/changelog`
automatically while preserving the existing Debian revision suffix:

```bash
./scripts/build-local-debs.sh -- -jauto
export GPG_TTY="$(tty)"
APT_REPO_SIGN_KEY=5D7762BDB9A2A564D500DE702A2E3C589C188616 \
  ./scripts/build-apt-repository.sh
./scripts/deploy-apt-repository.sh
```

Clients receive the update with the normal Ubuntu flow:

```bash
sudo apt update
sudo apt upgrade
```
