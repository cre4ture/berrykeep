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
Those files remain the source of truth for Rust dependencies. The generated
Crate Universe state is tracked by Bazel's standard `MODULE.bazel.lock`, rather
than a separate `Cargo.Bazel.lock`. Crate Universe's Cargo and rustc host tools
are pinned to Rust 1.88.0 for consistent resolution in CI.

After changing `Cargo.toml`, `Cargo.lock`, or `MODULE.bazel`, refresh the
checked-in Bzlmod lockfile and commit it with the source change:

```bash
bazel mod tidy --lockfile_mode=update
bazel test //:unit
```

Adding a first-party Rust package follows the pattern in
`crates/sync-core/BUILD.bazel`: use `crate_edition`, `aliases`, and
`all_crate_deps` from `@crates//:defs.bzl`, then add its test target to the
smallest appropriate root test suite. This lets Cargo define dependencies while
Bazel can invalidate only direct and transitive consumers of changed sources.

## Remote cache setup

The `Bazel` workflow uses
[`cre4ture/bazel-github-actions-cache-v2`](https://github.com/cre4ture/bazel-github-actions-cache-v2)
at an immutable commit SHA. The action starts an HTTP cache on the runner's
loopback interface and stores each validated Bazel AC/CAS object in GitHub
Actions cache v2. It therefore needs no external server, repository secret, or
manual repository setting.

The normal trust policy is automatic:

- a push to the default branch can publish cache entries;
- pull requests, including same-repository PRs, are read-only;
- fork pull requests remain read-only even if a workflow requests writes;
- a maintainer can explicitly select `write_cache` in a manual workflow run to
  seed that branch's cache before merge.

GitHub's cache scope rules make entries written on the default branch readable
from later pull requests. Different revisions do not replace one shared
archive: Bazel objects have immutable content/action keys and coexist until
GitHub's repository quota or eviction policy removes cold entries.

This adapter is intentionally experimental. GitHub does not promise its runner
cache-v2 upload/download protocol as a stable public API. The current adapter
also maps one Bazel object to one GitHub cache entry and rate-limits
publication, so a conventional remote cache remains the better option for a
large build graph. Cache misses and backend failures are fail-open by default
and never affect build correctness. See the action's README for current
protocol, quota, compression, and runner-platform limitations.

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
