# Bazel

Bazel is being introduced incrementally. Cargo, Gradle, Xcode and pnpm remain
the developer-facing and release build tools while native Bazel targets are
added package by package. This avoids a false whole-workspace Bazel target:
only a target whose sources, dependencies and test data are explicitly modeled
can benefit from Bazel's dependency-aware cache.

## Current scope

The current native targets are:

- `//crates/client-sdk:unit_test`
- `//crates/common:unit_test`
- `//crates/desktop-client-config:unit_test`
- `//crates/desktop-status:unit_test`
- `//crates/rendezvous-server:unit_test`
- `//crates/sync-agent-core:unit_test`
- `//crates/sync-core:unit_test`
- `//crates/transport-sdk:unit_test`

`transport-sdk` consumes the native `//crates/common:common` target, while
`rendezvous-server` consumes both native targets. `client-sdk` completes this
dependency layer by consuming `common`, `sync-core`, and `transport-sdk` at
runtime and `rendezvous-server` only in its unit tests. `desktop-status`
consumes the native `client-sdk` target, extending dependency-aware
invalidation to desktop integration code. `desktop-client-config` is an
independent leaf whose dependencies are all supplied by Crate Universe; its
native library prepares the later `background-launcher` and `config-app`
application targets. `sync-agent-core` builds on the native `client-sdk`,
`common`, and `sync-core` graph, and tracks its embedded folder agent UI files
as compile-time inputs. These targets avoid duplicate generated crates. Run
the current Bazel unit suite with:

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
are pinned to Rust 1.89.0 for consistent resolution in CI and parity with the
minimum compiler required by the current transport dependency graph.

After changing `Cargo.toml`, `Cargo.lock`, or `MODULE.bazel`, refresh the
checked-in Bzlmod lockfile and commit it with the source change:

```bash
bazel mod tidy --lockfile_mode=update
bazel test //:unit
```

Adding a first-party Rust package follows the pattern in
`crates/sync-core/BUILD.bazel`: use `crate_edition`, `aliases`, and
`all_crate_deps` from `@crates//:defs.bzl`, then add its test target to the
smallest appropriate root test suite. Crate Universe intentionally leaves
first-party workspace dependencies out of `all_crate_deps`; add their native
Bazel labels directly, as `transport-sdk` does for `//crates/common`. This lets
Cargo define dependencies while Bazel can invalidate only direct and transitive
consumers of changed sources.

## Remote cache setup

The `Bazel` workflow uses two cache layers. The maintained
[`bazel-contrib/setup-bazel`](https://github.com/bazel-contrib/setup-bazel)
action stores Bazelisk downloads, the Bazel disk cache, and the external
repository cache in GitHub Actions cache. Disk-cache entries are separated by
workflow and derived from the modeled build files. Pull requests restore these
caches without saving them, while trusted non-PR runs can refresh them.

The workflow also uses
[`cre4ture/bazel-github-actions-cache-v2`](https://github.com/cre4ture/bazel-github-actions-cache-v2)
at an immutable commit SHA. The action starts an HTTP cache on the runner's
loopback interface and stores each validated Bazel AC/CAS object in GitHub
Actions cache v2. It therefore needs no external server, repository secret, or
manual repository setting.

Fine-grained remote-cache publication is deliberately opt-in:

- normal default-branch pushes and all pull requests are read-only;
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
publication below GitHub's cache-creation limit. A cold native build was
observed creating thousands of entries while Bazel synchronously waited for
each bounded upload slot; the publication phase outlasted comparable read-only
runs by tens of minutes and contributed to repository-quota eviction. Raising
client concurrency cannot bypass that global creation limit and only increases
the pending upload and temporary-disk pressure. For this reason, routine CI
uses the coarse-grained `setup-bazel` cache and reads any surviving
fine-grained entries without publishing new ones.

Regular Bazel jobs are bounded to 30 minutes. An explicitly requested manual
fine-grained seed may run for up to 120 minutes so it cannot occupy an
unbounded runner while still allowing the current graph to drain its
rate-limited uploads. Cache misses and backend failures are fail-open by
default and never affect build correctness. See the action's README for
current protocol, quota, compression, and runner-platform limitations.

### Cache validation

Evaluate the two layers with separate workflow runs rather than treating a
successful seed as proof of a useful cache:

1. Run a normal read-only build and record Bazel elapsed time, action count,
   adapter hit/miss statistics, and the `setup-bazel` restore/save duration.
2. When a controlled fine-grained comparison is needed, dispatch one
   `write_cache` seed and record its published-object count, throttling,
   duration, and repository cache usage before and after the run.
3. Run the same revision, or a small descendant change, again without
   `write_cache`. Confirm that the disk cache restores, the adapter publishes
   no objects, and useful hits reduce total runtime.
4. Keep routine runs read-only unless the separate warm run demonstrates a
   repeatable gain without quota churn. If the per-object adapter remains the
   bottleneck, remove it from routine CI or replace its backend with a
   segmented format before enabling automatic writes.

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
