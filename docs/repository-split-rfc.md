# RFC: Split BerryKeep into contract, web, node, and OS-client repositories

Status: Proposed
Decision type: Architecture and migration plan

## Summary

BerryKeep will move from the current all-in-one repository to public repositories
with one owner for shared contracts, one for web source, one for node-side
software, one for platform-neutral client code, and one for each client
operating system. The migration preserves a single shared web UI source tree:
client platforms embed the same verified client bundle rather than copying or
forking it.

| Repository | Owns |
| --- | --- |
| berrykeep-contracts | Shared data models, API schemas, protocol definitions, generated bindings, and web-host/native-bridge version definitions. |
| berrykeep-node | Server Node, rendezvous and relay services, node storage, node telemetry, server packages, and cross-process system tests. |
| berrykeep-client-core | client-sdk, transport and sync implementations, target-neutral client services, and the local client Web-UI host. |
| berrykeep-web | The shared TypeScript/React web workspace, including client-ui, server-admin, and common web packages. |
| berrykeep-client-android | Android client application and Android-specific integration. The Android Server Node is deliberately excluded. |
| berrykeep-client-ios | iOS application and iOS File Provider integration. |
| berrykeep-client-linux | Linux client application, FUSE integration, desktop integration, and Linux client packaging. |
| berrykeep-client-windows | Windows client application, Cloud Files integration, Explorer thumbnail provider, and Windows client packaging. |
| berrykeep-client-macos | macOS client application and macOS File Provider integration. |
| berrykeep-cli (optional) | The platform-neutral CLI, if its release cadence warrants a separate repository. Until then it stays with berrykeep-client-core. |

This RFC does not split the Android Server Node into the Android client
repository. It remains node-side software in berrykeep-node.

## Motivation

The current workspace combines independent release cadences, platform SDKs,
frontend source, server packages, and their CI caches. This makes ownership
boundaries unclear and puts unrelated Rust, Node.js, and native-platform
workloads under one GitHub Actions cache quota.

The existing web workspace already has the intended source boundary:

- web/apps/client-ui
- web/apps/server-admin
- web/packages/ui
- web/packages/api
- web/packages/config

Its two consumers are also already distinct. server-node-sdk embeds the
server-admin bundle, and web-ui-backend hosts the client bundle. Android and
iOS start that client host on an authenticated loopback listener and display it
in a native WebView. The build scripts in server-node-sdk and web-ui-backend
already accept IRONMESH_PREBUILT_WEB_DIR, making a verified release-artifact
handoff practical without a runtime CDN or central web service.

## Goals

- Give each repository a clear, independently releasable responsibility.
- Keep API, protocol, web-host, and native-bridge compatibility explicit and
  independently testable.
- Preserve one source of truth for the client web UI across all operating
  systems.
- Allow Node and client releases to embed immutable, hash-verified web
  artifacts and work offline at runtime.
- Keep the migration incremental: each extracted repository must build and
  release before the next extraction depends on it.
- Reduce cache pressure by distributing independent workloads across
  repositories, while measuring and configuring each repository deliberately.

## Non-goals

- This RFC does not change the user-visible protocol, storage format, or
  authentication model.
- This RFC does not combine the Server Node admin API with the client local
  Web-UI-host API.
- This RFC does not introduce a hosted web application, CDN, git submodules,
  or runtime downloads of UI code.
- This RFC does not prescribe signing-provider implementation details. Signing
  material must remain in protected secret stores and must not be transferred
  as repository content.
- This RFC does not perform the extraction itself; it defines the order and
  acceptance conditions for focused follow-up PRs.

## Architectural decision

### Repository ownership and dependency direction

The following graph is the permitted high-level dependency direction. An arrow
means "may consume a released, compatible dependency from". Source-tree,
relative-path, and git-revision dependencies across repositories are not
permitted after the relevant migration phase is complete.

~~~text
                 berrykeep-contracts
                    ^       ^       ^
                    |       |       |
       berrykeep-web |  berrykeep-node  berrykeep-client-core
                    |       ^                ^
                    |       |                |
        verified web bundles |                |
                    |       |                |
                    +-------+----------------+
                            ^
                            |
     Android / iOS / Linux / Windows / macOS client repositories
~~~

More precisely:

- berrykeep-contracts has no dependency on product repositories. It is the
  normative source for wire-level and API-level compatibility.
- berrykeep-client-core depends on contracts. It may publish target-neutral
  transport packages that berrykeep-node consumes; it must not depend on a
  platform client.
- berrykeep-node depends on contracts and, where needed, released
  target-neutral transport packages from client core. It never depends on an
  OS client or on web source.
- berrykeep-web depends on generated TypeScript contracts, not on Rust source.
  It publishes static artifacts, not a runtime service.
- Each OS client depends on contracts and client core, and consumes the
  client-ui artifact. It must not copy web source or depend on another OS
  client.
- The Server Node consumes the server-admin artifact. It has no reason to
  consume the client-ui artifact in its final build.

Dependencies use released versions and integrity locks. During a transition,
the original workspace may retain temporary local paths only until the
consumer has successfully switched to a released package or artifact; such
paths are removed in the same extraction PR or its immediately following
consumer PR.

### Contracts repository

berrykeep-contracts contains language-neutral, reviewable definitions plus
generated Rust and TypeScript bindings. Its initial contents include:

- stable identifiers and shared data models currently embedded in crates/common;
- public Server Node admin API schemas;
- local client Web-UI-host API schemas;
- bootstrap, rendezvous, relay, and transport protocol records that cross a
  process or repository boundary;
- the web-bundle manifest schema and the native-bridge capability schema;
- compatibility fixtures and conformance tests.

crates/common is not moved wholesale. Identity and serializable model types
move to contracts. Logging, lock wrappers, caches, and other runtime utilities
remain private to their consumer or are split into package-private utilities.
This avoids making a generic utility crate an accidental public protocol.

The contracts release version is semantic versioning:

- A patch release clarifies schemas or generated bindings without changing
  accepted data.
- A minor release adds optional fields, endpoints, messages, or capabilities
  with documented defaults.
- A major release removes support or changes the interpretation of existing
  data. A network API breaking change receives a new explicit API version;
  reinterpreting an existing version is forbidden.

Every contract release publishes machine-readable schemas, generated bindings,
and fixtures from the same source revision. Consumers pin an exact release or
a declared compatible range according to their package ecosystem, and CI tests
both the lowest supported and current compatible contract versions.

### Separate web backends

The two web applications remain separate because their security boundaries and
backends differ:

| Application | Backend owner | Runtime peer |
| --- | --- | --- |
| server-admin | berrykeep-node | Server Node admin and setup APIs |
| client-ui | berrykeep-client-core | Local client Web-UI host, then client-sdk, direct transport, and relay |

They share only contract-generated types, design-system components, feature
code that has no backend assumption, and configuration helpers. A component or
feature that imports a server-admin API client cannot be imported by the
client UI, and vice versa.

Within berrykeep-web, the initial package layout is:

- packages/design-system — theme, primitives, accessibility, and common UI
  components (renamed from the current packages/ui when extracted);
- packages/shared-features — backend-independent page and feature helpers;
- packages/api-contracts — generated TypeScript bindings and typed adapters
  for the versioned contracts;
- packages/native-bridge — the typed client-side bridge handshake and
  capability adapter;
- packages/config — build and runtime configuration shared by the apps.

No native OS implementation is placed in the web repository.

## Web bundle release contract

Each berrykeep-web release produces exactly these immutable release assets:

- berrykeep-client-ui-<version>.tar.zst
- berrykeep-server-admin-<version>.tar.zst
- berrykeep-web-bundles-<version>.json
- SHA256SUMS

The manifest is the canonical consumption record. Its first schema version
includes the release version and source revision, and one entry per bundle with
all of the following fields:

- artifact filename, component name, SHA-256, compressed size, and archive
  layout root;
- supported contract version ranges;
- supported local Web-UI-host API or Server Node admin API version ranges, as
  applicable;
- the required and optional native-bridge capability versions for client-ui;
- build metadata sufficient to trace the output to the source release.

The archive contains only the static application distribution under its stated
layout root. The release workflow builds from the locked package graph, runs
type checks and application tests, creates deterministic archives where the
toolchain allows it, verifies the archive layout, and computes the published
checksums before uploading assets.

Each consumer repository commits a small bundle lock file containing the
artifact URL, exact web release version, SHA-256, manifest schema version, and
the selected manifest entry. Its build preparation step downloads the asset
only when preparing a build environment, verifies SHA-256 before extraction,
rejects unsafe archive paths, and exposes the resulting directory through the
current interface:

~~~text
$IRONMESH_PREBUILT_WEB_DIR/client-ui
$IRONMESH_PREBUILT_WEB_DIR/server-admin
~~~

The Rust build scripts must remain strict when that environment variable is
set: a missing index.html, an integrity mismatch, or an unsupported manifest
is a build failure. If the variable is absent only during the transition,
developer builds may retain the existing source-workspace fallback. Release
and CI builds switch to required prebuilt artifacts before the web repository
is extracted; the fallback is removed once all consumers have migrated.

The resulting Server Node and client packages embed the extracted assets. They
never retrieve UI code from a CDN, GitHub Release, or another web server at
application runtime.

## Native-bridge contract

The client UI treats native functionality as a versioned set of capabilities,
not as Android- or Apple-specific JavaScript. berrykeep-contracts defines a
base handshake and JSON request/response schemas; packages/native-bridge
provides the TypeScript implementation used by client-ui.

At startup, an authenticated local host exposes a bridge descriptor containing
the bridge contract major/minor version, platform identifier, and a set of
named capabilities with supported versions. Initial capabilities include
share/export, download handoff, file selection, and fullscreen presentation.
Each request has a typed input and a typed success or error result. Features
declare the exact capability version they require.

- An unsupported optional capability is hidden or disabled with an explanatory
  local UI state.
- A required capability or incompatible bridge major version prevents that
  feature from starting; it must not silently invoke a browser-level fallback
  that weakens the platform security model.
- Adding an optional capability or optional request field is a minor change.
  Removing a capability, changing required behavior, or changing a payload
  incompatibly requires a new bridge major version.

Native implementations expose the bridge only to the active authenticated
loopback WebView origin. They keep the existing lifecycle protections: a new
short-lived authorization for every presentation, no authorization in URLs,
restricted navigation, and teardown on dismissal, backgrounding, or identity
replacement. The bridge must not become a generic JavaScript-evaluation or
arbitrary-file API.

## Current workspace mapping

The mapping describes final ownership, not an instruction to copy all paths at
once. A mixed directory is split by the listed product boundary. Existing
history is preserved with a filtered extraction or a documented import commit;
the target repository remains authoritative after the cutover.

| Current path | Target repository | Migration treatment |
| --- | --- | --- |
| crates/common | berrykeep-contracts, berrykeep-client-core, and berrykeep-node | Split serializable IDs/models into contracts; retain runtime utilities with their actual consumers. |
| crates/client-sdk, crates/transport-sdk, crates/sync-core, crates/sync-agent-core | berrykeep-client-core | Publish target-neutral crates; extract protocol records to contracts where Node also consumes them. |
| crates/web-ui-backend | berrykeep-client-core | Keep the local client Web-UI host and its authenticated loopback serving model here. |
| crates/server-node-sdk, crates/rendezvous-server, crates/stats-collector-server | berrykeep-node | Keep Server Node, relay/rendezvous, storage, admin APIs, and node telemetry together initially. |
| apps/server-node, apps/rendezvous-service, apps/android-server-node-app, apps/tiny-display-status | berrykeep-node | Android Server Node stays node-side; hardware status belongs with the node it observes. |
| apps/cli-client | berrykeep-client-core, then optionally berrykeep-cli | Keep it with core until a separate release cadence, ownership, and packaging pipeline justify extraction. |
| apps/android-app | berrykeep-client-android | Move Android app, Gradle project, JNI integration, WebView implementation, and Android tests. |
| apps/ios-app | berrykeep-client-ios | Move iOS Rust facade, FFI, and iOS-specific tests. |
| apps/apple-file-provider | berrykeep-client-ios and berrykeep-client-macos | Split iOS and macOS targets, entitlements, runners, and File Provider code into their respective OS repositories. Extract target-neutral FFI bindings to a versioned core package; any remaining Apple-platform sharing needs an explicit follow-up ownership decision rather than a copied source tree. |
| apps/folder-agent | berrykeep-client-core and berrykeep-client-linux | Keep the OS-independent folder-sync agent in client core; move its GNOME shell extension and Linux integration to Linux. |
| apps/ironmesh-folder-agent | berrykeep-client-linux | Classify this legacy GNOME-only source during extraction, then either move it with the Linux integration or remove it after its replacement is verified. |
| crates/adapter-linux-fuse and Linux portions of apps/os-integration, apps/config-app, apps/background-launcher, crates/desktop-status, crates/desktop-client-config | berrykeep-client-linux | Separate Linux desktop/FUSE service and packaging responsibilities from target-neutral client core code. |
| crates/adapter-windows-cfapi, crates/windows-client-config, crates/windows-thumbnail-provider, and Windows portions of apps/os-integration, apps/config-app, apps/background-launcher, crates/desktop-status, crates/desktop-client-config | berrykeep-client-windows | Move Cloud Files, Explorer integration, configuration UI, launch integration, and Windows tests. |
| Target-neutral portions of apps/config-app, apps/background-launcher, crates/desktop-status, and crates/desktop-client-config | berrykeep-client-core | Extract shared service/configuration models and interfaces before the Linux and Windows implementations; platform adapters must not remain as hidden core dependencies. |
| macOS portions of apps/os-integration, apps/config-app, apps/background-launcher, crates/desktop-status, and crates/desktop-client-config | berrykeep-client-macos | Extract only when a native macOS client host is ready; do not create a source fork from iOS. |
| web/apps/client-ui, web/apps/server-admin, web/packages, web/tests, web/package.json, web/pnpm files, web/tsconfig files, web/vite files, Playwright configuration | berrykeep-web | Move as one pnpm workspace; replace package names and project documentation as part of the extraction. |
| tests/system-tests | berrykeep-node initially; contracts for conformance fixtures | Keep node-led multi-process end-to-end tests in Node using released dependencies. Move schema fixtures and interoperability suites to contracts; platform regressions stay with their OS repository. |
| debian | berrykeep-node and berrykeep-client-linux | Split Server Node/rendezvous packages into Node and Linux-client packages into the Linux repository; do not keep a cross-repository Debian source package. |
| macos/server-node | berrykeep-node | It packages a server component for macOS, not the macOS client. |
| windows/server-node-installer | berrykeep-node | It packages the Server Node for Windows, not the Windows client. |
| windows/thumbnail-provider, assets/windows, build-support/windows_icon_build.rs | berrykeep-client-windows | Move Explorer provider packaging and Windows client visual/build assets together. |
| scripts and start_node.sh | Owning repository | Split by invoked product: node build/deploy/telemetry scripts to Node; client platform scripts to that client; web build/release scripts to Web. Remove scripts that cross an old workspace boundary. |
| .github/actions, .github/workflows, .github/pull_request_template.md | Each target repository | Recreate repository-local CI/release workflows and only reusable actions needed by that repository. Do not retain a monorepo-wide workflow dependency. |
| Cargo.toml, Cargo.lock, .cargo, MODULE.bazel, MODULE.bazel.lock, BUILD.bazel, .bazelrc, .bazelversion, justfile, rust-toolchain.toml, deny.toml | Each Rust-owning repository | Create independent manifests, locks, toolchain policy, and build metadata from the sources they own. No root workspace lock survives as a shared dependency mechanism. |
| docs | Owning repository; contracts for cross-repository compatibility documentation | Move operational/node, web, and OS-specific documents with their code. Keep normative compatibility and migration documents in contracts; archive this RFC in the legacy repository. |
| README.md, LICENSE, SECURITY.md, .githooks, AGENTS.md, PACE.md, .codex, CLAUDE.md, .vscode, .idea, .gitignore, TODO.txt | Each target repository or no migration | Recreate repository policy and development files as appropriate; editor state and obsolete task notes are not product artifacts. Every public repository carries its applicable license and security policy. |
| Release credentials, certificates, and other signing inputs | Protected secret storage only | Do not move these as source content. Configure each repository's release environment independently. |

## Migration sequence

Each phase is a reviewable PR series with a green release candidate before the
next phase begins.

1. **Stabilize contracts in the monorepo.** Define language-neutral API,
   protocol, Web-bundle-manifest, and native-bridge schemas. Add generated
   Rust/TypeScript bindings, fixtures, compatibility tests, and a documented
   support policy. Do not change behavior while moving definitions.
2. **Create and release berrykeep-contracts.** Publish the first compatible
   packages/schemas and update monorepo consumers to use the generated
   interfaces at the same semantics.
3. **Extract berrykeep-web first.** Move the complete pnpm workspace, build
   both applications, and publish the two verified artifacts plus manifest.
   Add consumer bundle lock files and make CI use
   IRONMESH_PREBUILT_WEB_DIR; prove a build succeeds without web source or
   node_modules in the consumer checkout.
4. **Extract berrykeep-client-core.** Release the client SDK, target-neutral
   transport and sync packages, and the local Web-UI host. Replace all
   remaining cross-workspace path dependencies with released packages.
5. **Extract Android first.** Move only apps/android-app and its Android
   dependencies, pin the shared client-ui artifact, implement the versioned
   bridge, and validate native WebView lifecycle and offline embedding.
6. **Extract iOS, Windows, Linux, and macOS independently.** For each client,
   first establish its package/CI boundary, then move native integration,
   consume released core and web artifacts, and add platform-specific
   conformance coverage. The Apple project is split by target rather than
   copied wholesale.
7. **Extract berrykeep-node.** Move the Server Node, rendezvous, telemetry,
   server packages, and node-owned system tests. Pin the server-admin artifact
   and consume only released contracts/transport packages.
8. **Decide on berrykeep-cli.** Extract the CLI only if its release and
   support model is independent of client core; otherwise keep it in core.
9. **Retire the monorepo.** Once every consumer builds from released inputs,
   make the old repository read-only or archive it with this RFC, import
   references, and a migration guide. Remove temporary source fallbacks and
   local path dependencies before declaring the split complete.

For every extraction, the cutover PR must include an ownership list, a tested
rollback procedure, replacement issue links for deferred files, and a release
tag or reproducible release candidate. Repositories should be created with the
same public-license, security-reporting, branch-protection, and dependency
update baseline before code is imported.

## CI, cache, and release implications

GitHub Actions Cache capacity is repository-local. Splitting repositories
therefore separates the cache pressure of Cargo/rust-cache, sccache, Bazel,
Node.js, and platform toolchains. It does not make a cache garbage collector
inside one cache backend sufficient by itself.

The pre-split measurement on 2026-08-20 was approximately 11.61 GB across
7,509 entries, including about 4.34 GB of Cargo/rust-cache, 4.29 GB of
sccache, and 1.72 GB of Bazel PAC data. All major entries were current main
entries. The existing PR cleanup workflow only cleans closed-PR scopes; it
does not bound these default-branch caches. The split addresses the
repository-wide contention, but each new repository still needs a measured
budget and retention policy, especially for Cargo and sccache.

Before enabling writes in a new repository:

1. Measure cache entries and bytes after a cold seed and after at least one
   warm build.
2. Keep pull-request jobs restore-only and let trusted default-branch or
   release jobs seed reusable caches.
3. Bound or prune default-branch Cargo and sccache caches based on measured
   value, separately from PR cleanup.
4. Use the Bazel GitHub Actions Cache v2 action only in repositories with a
   Bazel suite that benefits from it. Its immutable-pack/DAG design remains
   fail-open and local to the repository; it does not reduce Cargo or sccache
   usage.
5. Record cache size, hit rate, and build duration in the release checklist
   for the first three releases of each repository.

Each repository owns CI appropriate to its products: contracts run schema,
generator, and compatibility tests; web runs locked package, type, build, and
browser tests; core runs Rust tests and local-host integration tests; each OS
repository runs its native checks; and Node runs server package and
cross-process system tests. Cross-repository integration jobs consume released
inputs rather than checking out mutable sibling repositories.

To bootstrap the split, publish contracts first, then the initial web artifacts,
then the client-core and Node releases that consume those artifacts. After
cutover, a change that raises a host requirement releases contracts when
needed, then the implementing core or Node package, then the web artifact, and
finally the client or Node package that embeds it. A consumer release may use a
newer web bundle only after manifest compatibility validation succeeds.
Publishing a new web bundle never changes the UI embedded in an already
released client or Server Node package.

## Acceptance criteria

The repository split is complete only when all of the following are true:

- Every target repository has an explicit owner, release workflow, security
  policy, cache measurement, and branch-protection configuration.
- Contract schemas, generated bindings, bundle manifests, and bridge fixtures
  are released and compatibility-tested.
- berrykeep-web publishes both immutable bundles and their hash-checked
  manifest on every release.
- Server Node and every client build consume a committed, verified artifact
  lock and embed the result without a runtime network dependency.
- server-admin and client-ui remain separate applications and use only their
  respective backend APIs.
- Every native client reports and tests its bridge capabilities, including
  rejection of incompatible major versions.
- There are no cross-repository local-path, submodule, or moving git-revision
  dependencies in release builds.
- Platform, node, and conformance test ownership has moved with the owning
  code, and cross-repository tests use released inputs.
- Default-branch Cargo and sccache retention is explicitly bounded or
  justified by measurements in every new repository.

## Alternatives considered

### Copy the web workspace into each OS repository

Rejected. It would cause divergent UI behavior, duplicate dependency and
security updates, make bridge changes harder to coordinate, and multiply
frontend build cache use. A single berrykeep-web source tree plus verified
offline artifacts preserves shared behavior without requiring a runtime web
service.

### Host one central web application

Rejected. The current product must work offline and retains a native,
authenticated loopback-host security model. Static assets embedded in released
packages meet that requirement.

### Keep all shared Rust code in a generic foundation repository

Rejected for the initial split. It would create an overly broad dependency root
and blur protocol versus implementation ownership. Only normative shared
contracts are centralized; target-neutral transport code has an explicit,
limited package interface in client core.

### Solve cache pressure only with PAC garbage collection

Rejected. PAC cache entries were not the dominant source of the measured
capacity issue, and a fresh seed contains no old PAC duplicates to collect.
Cargo/rust-cache and sccache need their own retention controls; repository
boundaries additionally prevent unrelated workloads from competing for one
quota.
