# Bazel

Bazel is being introduced incrementally. Cargo, Gradle, Xcode and pnpm remain
the developer-facing and release build tools while native Bazel targets are
added package by package. This avoids a false whole-workspace Bazel target:
only a target whose sources, dependencies and test data are explicitly modeled
can benefit from Bazel's dependency-aware cache.

## Current scope

`//crates/sync-core:unit_test` is the first native target. Run the current
Bazel unit suite with:

```bash
bazel test //:unit
```

The suite is intentionally additive to the established Cargo and platform CI
checks until its Bazel equivalents are present and have demonstrated parity.

## Tooling and dependency updates

Install [Bazelisk](https://github.com/bazelbuild/bazelisk) as `bazel`; it
selects the pinned version from `.bazelversion`. Bazel uses `MODULE.bazel` and
`rules_rust`'s Crate Universe to read the root `Cargo.toml` and `Cargo.lock`.
The Cargo files remain the source of truth for Rust dependencies.

After changing `Cargo.toml` or `Cargo.lock`, regenerate the renderer lockfile
and commit it with the Cargo changes:

```bash
CARGO_BAZEL_REPIN=1 bazel build //:unit
```

Adding a first-party Rust package follows the pattern in
`crates/sync-core/BUILD.bazel`: use `crate_edition`, `aliases`, and
`all_crate_deps` from `@crates//:defs.bzl`, then add its test target to the
smallest appropriate root test suite. This lets Cargo define dependencies while
Bazel can invalidate only direct and transitive consumers of changed sources.

## Remote cache setup

The `Bazel` workflow operates without a remote cache, but consumes one as soon
as the following repository settings exist:

1. Provision an HTTPS or `grpcs` endpoint that implements Bazel's remote-cache
   protocol (for example a managed service or a maintained `bazel-remote`
   deployment).
2. Add its endpoint as the repository variable `BAZEL_REMOTE_CACHE_URL`.
3. If authentication is required, add a bearer token as the repository secret
   `BAZEL_REMOTE_CACHE_TOKEN`.
4. Grant the token read/write access only to trusted repository CI. PRs from
   forks do not receive repository secrets and deliberately fall back to local
   execution, preventing cache poisoning.
5. Set retention and a storage quota at the cache backend. The cache is shared
   across revisions by action-content hash, so do not partition it by branch.

The workflow deliberately does not include a cache URL, credential or hosted
service choice. Those are infrastructure decisions outside the repository and
must be supplied by a maintainer with access to the selected backend.

## Migration order

1. Add native Rust libraries and unit tests, starting with leaf crates and
   moving toward their consumers.
2. Add platform triples as their native targets arrive, then model build
   scripts, binaries and platform targets once their inputs are hermetic.
3. Add frontend build and test targets, then Android and iOS.
4. Move an existing CI check to Bazel only after its Bazel test suite is
   complete and observed to be equivalent.

Networked system tests and signing/release jobs should remain explicit,
non-cacheable targets even after the rest of their build graph is Bazelized.
