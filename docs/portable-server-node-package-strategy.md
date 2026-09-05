# Portable Static Server-Node Package Strategy

Status: Generic static artifact pipeline implemented; portable package and
repository convergence in progress.

Related notes:

- [Self-Hosted Apt Repository](self-hosted-apt-repository.md)
- [Ubuntu PPA Packaging](ubuntu-ppa-packaging.md)
- [Server-Node Hardware Health Strategy](server-node-hardware-health-strategy.md)
- [Automatic Natural Earth Import](natural-earth-automatic-import-concept.md)

## Decision summary

BerryKeep builds one generic, fully static musl Server Node executable per
supported Linux CPU ABI. The same verified executable is reused when testing
and packaging for Debian, Ubuntu, Raspberry Pi OS, or another compatible
Debian-family distribution. Distribution suites no longer define the server
binary build matrix.

The initial portable variants are:

| Package architecture | Variant ID | Rust target | CPU contract |
| --- | --- | --- | --- |
| `amd64` | `x86_64-generic` | `x86_64-unknown-linux-musl` | Generic x86-64 Linux baseline |
| `arm64` | `aarch64-generic` | `aarch64-unknown-linux-musl` | Generic ARMv8-A AArch64 baseline |

The existing `armv7-cortex-a7` helper remains available for the LuckFox and
other explicitly validated ARMv7 deployments. It is a specialized binary, not
yet a portable `armhf` package baseline. A generic ARMv7 artifact and any
runtime selector remain deferred until the hardware matrix and benchmarks
justify their permanent release cost.

This Generic-first plan supersedes the earlier assumption that the first
portable package must contain several complete CPU variants and a selector.
One complete generic binary per ABI is the simplest useful portability step.
Additional optimized binaries may be added later without changing the static
artifact contract described here.

## Goals

- Compile the Server Node once per CPU ABI, not once per distribution suite.
- Remove the server executable's glibc and shared-library ABI dependency.
- Keep `ironmesh-server-node` as the public executable, package, service, user,
  configuration, and state-directory name during the BerryKeep rename.
- Preserve ordinary signed apt installation and update behavior.
- Make every published artifact independently inspectable and reproducible
  from its recorded target, CPU setting, source revision, and checksum.
- Keep optional host tools out of the core server package dependency closure.

## Non-goals

- Making the desktop client, FUSE mount, or rendezvous package portable in this
  work.
- Producing one executable that runs on different CPU architectures.
- Replacing apt or systemd with a custom updater or service manager.
- Bundling GDAL, FFmpeg, smartmontools, or other external programs into the
  Rust executable.
- Adding CPU-specific variants without repeatable benchmarks and real-hardware
  validation.

## Implemented artifact pipeline

`scripts/build-static-server-node.sh` is the canonical builder. It:

1. accepts an explicit supported musl target and artifact variant ID;
2. installs a checksum-pinned Zig 0.16.0 toolchain for the build host;
3. uses the repository's pinned Rust toolchain and `cargo zigbuild`;
4. applies `panic=abort` only to this standalone server build;
5. rejects ambient `RUSTFLAGS` so a generic artifact cannot accidentally
   inherit host-specific CPU options;
6. checks the ELF machine, program interpreter, and dynamic `DT_NEEDED`
   entries;
7. runs `ironmesh-server-node --version` when the target matches the host;
8. writes a tar archive containing the executable, `SHA256SUMS`, and
   `build-metadata.json`, plus a checksum for the archive.

Example builds:

```bash
./scripts/build-static-server-node.sh \
  --target x86_64-unknown-linux-musl \
  --variant-id x86_64-generic

./scripts/build-static-server-node.sh \
  --target aarch64-unknown-linux-musl \
  --variant-id aarch64-generic
```

Artifacts are written below `target/static-server-node/`. The metadata records
the package version, Git revision, dirty-source state, variant ID, Rust target,
target CPU, and executable checksum.

The old `scripts/build-server-node-armv7-musl.sh` entry point is retained as a
compatibility wrapper. It delegates to the canonical builder with
`armv7-unknown-linux-musleabihf`, `target-cpu=cortex-a7`, and variant ID
`armv7-cortex-a7`, so the LuckFox deployment path keeps its existing tuning.

## Current CI integration

The normal CI workflow builds and verifies `x86_64-generic` on an x86_64
GitHub runner. The static build is part of the `Required CI` aggregate and
uploads `static-server-node-linux-amd64`.

The Server Node Debian package workflow cross-builds and verifies
`aarch64-generic` on an x86_64 GitHub runner with Zig, then uploads
`static-server-node-linux-arm64`. The static artifact checks its ELF contract;
the workflow runs its version smoke test under `qemu-aarch64-static` before
the artifact enters a package container.

The workflow passes that archive into Focal and Trixie containers. Each
container checks the archive checksum and metadata against the checked-out Git
revision, then cross-assembles an `arm64` `ironmesh-server-node` package with
the `server-node-only` build profile. The profile skips source compilation and
does not execute the target binary, so the package-wrapper matrix no longer
needs an ARM runner.

The final protected, manual CI job imports the archive key into a temporary
keyring, signs all suite metadata, deploys the matrix, and re-verifies the
remote and public `InRelease` copies. Client and rendezvous packages remain on
their distribution-specific native package path.

## Portable package contract

The core package remains:

```text
ironmesh-server-node
```

It contains the static server executable, systemd unit, environment template,
dedicated system user integration, and existing state-directory contract. The
binary itself has no glibc or other shared-library dependency. Debian
`${shlibs:Depends}` remains enabled for the distribution-native compatibility
path, while the static artifact builder is the hard gate that rejects a future
ELF interpreter or shared-library dependency.

Natural Earth conversion is optional server functionality. Its host tools are
therefore provided through a companion metapackage:

```text
ironmesh-server-node-map-tools
  Depends: ironmesh-server-node (= ${binary:Version}), gdal-bin, unzip
```

Installing only `ironmesh-server-node` provides the complete storage server.
Installing `ironmesh-server-node-map-tools` additionally enables the GDAL- and
unzip-backed Natural Earth import workflows. The server's dependency-health
API continues to report whether those commands are available.

Other optional integrations remain host-provided:

- `ffmpeg` and `ffprobe` enable video inspection and thumbnail extraction;
- `smartctl` enriches hardware-health information;
- the system trust store supplies native CA certificates when external HTTPS
  endpoints require them.

Static linking does not bundle those programs or operating-system data files.

## Host compatibility boundary

A portable artifact still has an explicit host contract:

- a Linux kernel and CPU implementing the selected ABI baseline;
- a Debian-family userspace for the `.deb`, systemd, and apt integration;
- writable configuration and state paths with the existing ownership model;
- optional external tools when their corresponding feature is used.

The same `aarch64-generic` executable is intended for 64-bit Raspberry Pi OS,
Ubuntu ARM64, and comparable AArch64 Debian-family hosts. It does not run on a
32-bit `armhf` installation. Likewise, the x86_64 artifact is a separate build,
not a fat or multi-architecture executable.

## Repository model

During migration, the signed repository retains its existing `focal` and
`noble` suites so installed systems continue to update normally. Their Server
Node packages now contain static binaries, but the complete source package
still includes distribution-specific client and rendezvous outputs.

The target end state is a product-owned suite such as:

```text
dists/stable/main/binary-amd64/
dists/stable/main/binary-arm64/
```

`stable` identifies the BerryKeep release channel rather than the host
distribution. A later packaging step will build the Server Node `.deb` once
per package architecture from the verified static artifact and index that
identical package in every supported suite during the transition.

## Delivery status and next phases

### Phase 1 — Generic static artifacts: implemented

- [x] Generalize the ARMv7 helper into one target-explicit static builder.
- [x] Build generic x86_64 and AArch64 musl artifacts in CI.
- [x] Verify ELF architecture, static linkage, checksums, and version output.
- [x] Record source and target metadata beside every artifact.
- [x] Reuse the static artifacts in the current AMD64 and ARM64 package jobs.

### Phase 2 — Portable core package: partially implemented

- [x] Remove GDAL and unzip from the core package dependency closure.
- [x] Add the optional `ironmesh-server-node-map-tools` metapackage.
- [x] Allow a verified prebuilt Server Node to be combined with source-built
  client and rendezvous packages.
- [ ] Add a dedicated package job that builds only the portable Server Node
  package once per architecture.
- [ ] Add install, start, and upgrade tests in clean Debian, Ubuntu, and
  Raspberry Pi OS-compatible images using the same `.deb` file.

### Phase 3 — Portable repository channel

- [ ] Publish portable packages in the signed `stable` suite.
- [ ] Keep `focal` and `noble` as compatibility entries during migration.
- [ ] Verify that existing configuration, system user ownership, and node data
  survive migration from the dynamically linked package.
- [ ] Decide when the distribution-named compatibility suites can be retired.

### Phase 4 — Optional optimized variants

- [ ] Establish repeatable hashing, ingest, replication, startup, memory, and
  binary-size benchmarks.
- [ ] Produce a generic ARMv7 baseline before publishing an `armhf` package.
- [ ] Retain a specialized CPU variant only when real hardware demonstrates a
  material benefit.
- [ ] Introduce a safe HWCAP-based selector only if one package architecture
  genuinely needs more than one complete executable.

## CI and verification matrix

| Check | Frequency | Purpose |
| --- | --- | --- |
| Static x86_64 build | every normal CI run | produces and executes the generic AMD64 artifact |
| Static AArch64 build | ARM64 package workflow | produces and executes the Raspberry Pi/ARM64 artifact |
| ELF and checksum verification | every static build | rejects dynamic or mislabeled artifacts |
| Debian package build | current package workflows | proves the static server can be packaged with existing components |
| Cross-distribution install/start tests | Phase 2 | proves one package works without recompilation |
| Real ARM64 hardware smoke test | release candidates | validates kernel, CPU, and operational behavior |

The expensive server compilation matrix is `(CPU ABI)`, not `(distribution
suite, CPU ABI)`.

## Security and reliability rules

- Artifact targets are explicit and allow-listed; variant IDs are explicit and
  syntax-validated.
- Generic builds reject ambient target flags that could silently raise their
  CPU baseline.
- CI treats any ELF interpreter or `DT_NEEDED` entry as a build failure.
- Artifact archives include source revision and checksums; package jobs consume
  the verified executable rather than rebuilding it.
- Package maintainer scripts never download software or invoke a nested package
  manager transaction.
- Optional tools remain visible through package metadata and runtime health
  diagnostics.

## Open decisions

1. Should `stable` become the sole public apt suite, or remain an alias beside
   distribution-named suites?
2. Which Linux kernel versions form the minimum supported runtime baseline?
3. Should `ca-certificates` become an explicit core-package dependency?
4. What real Raspberry Pi and generic ARM64 hosts form the permanent release
   validation pool?
5. Is a generic ARMv7 package still valuable after the ARM64 rollout?

## Deferred and rejected alternatives

### Multi-variant package before generic portability

Shipping several complete binaries and a selector remains technically viable,
but it multiplies package size, hardware validation, and fallback behavior.
The project first ships one generic binary per ABI. Optimized variants require
benchmark evidence and do not block distribution-neutral builds.

### Fat binary or in-process function multiversioning

This would make the CPU capability boundary interact with Cargo feature
unification, LTO, and transitive native dependencies. If optimization is later
needed, complete separately verified executables plus a small HWCAP selector
remain easier to inspect and roll back.

### Separate public flavor packages and an external installer

This adds package selection and update orchestration outside normal apt
updates. Portable generic packages preserve the ordinary package name and
upgrade path.

### Bundling external media and map tools

Statically embedding GDAL, FFmpeg, or smartmontools would substantially enlarge
the artifact and complicate their security-update lifecycle. Distribution
packages remain the appropriate delivery mechanism for those optional tools.
