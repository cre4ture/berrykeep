# macOS Server-Node Package

The macOS server-node distribution is a headless `launchd` `LaunchDaemon`.
It is intended for a Mac that should host a node independently of any logged-in
desktop user. The package uses a dedicated non-login `_ironmesh` account, keeps
state under `/Library/Application Support/Ironmesh/server-node`, and runs the
node as that account rather than as `root`.

This is distinct from a user-scoped `LaunchAgent`: a `LaunchAgent` is suitable
for development tools tied to a login session, whereas this daemon starts at
boot and continues after all users log out.

## Build a package

Builds must run on macOS with Xcode Command Line Tools, Rust, Node.js, and pnpm
available. The server-node build embeds the server-admin web UI, so install the
web workspace dependencies first:

```bash
pnpm --dir web install --frozen-lockfile
./scripts/build-macos-server-node-pkg.sh
```

The default package contains a native binary for the build Mac's architecture
and is written below `target/macos/`. To make a universal Apple Silicon + Intel
package, install both Rust targets and use `lipo` through the same helper:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
./scripts/build-macos-server-node-pkg.sh --arch universal
```

For distribution outside a controlled development environment, sign both the
executable and package with the appropriate Developer ID identities, then
submit the resulting package to Apple's notarization service as part of the
release pipeline:

```bash
./scripts/build-macos-server-node-pkg.sh \
  --arch universal \
  --code-sign-identity 'Developer ID Application: Example Org (TEAMID)' \
  --installer-sign-identity 'Developer ID Installer: Example Org (TEAMID)'
```

The helper also accepts `--binary PATH` to package a prebuilt release binary.
This is used by the package-structure regression test and avoids rebuilding
Rust for package-only validation.

## Install and configure

Install the generated or released package on the target Mac:

```bash
sudo installer -pkg target/macos/ironmesh-server-node-<version>.pkg -target /
```

The installer creates the `_ironmesh` non-login account, starts the daemon, and
preserves both configuration and server data when the package is upgraded. The
initial configuration is copied only once to:

```text
/Library/Application Support/Ironmesh/server-node.env
```

It listens on `127.0.0.1:8443` by default. Before exposing the node on the
network, edit the configuration and set an appropriate bind address, public
URL, TLS material, and administrative token:

```bash
sudoedit '/Library/Application Support/Ironmesh/server-node.env'
sudo launchctl kickstart -k system/io.ironmesh.server-node
```

The configuration file is deliberately not sourced as a shell program. It
accepts literal `IRONMESH_*` runtime settings and `RUST_LOG` only. Comments
must begin in the first column, and shell quoting, interpolation, and command
substitutions are not supported.

## Operate the service

`launchd` owns the lifetime of the daemon. It starts at boot and restarts after
an unsuccessful exit, with a five-second restart throttle.

```bash
# Inspect the loaded job and its last exit status.
sudo launchctl print system/io.ironmesh.server-node

# Restart after changing configuration.
sudo launchctl kickstart -k system/io.ironmesh.server-node

# Follow process output written by launchd.
sudo tail -f /Library/Logs/Ironmesh/server-node.stderr.log
```

The installed files are:

| Path | Purpose |
| --- | --- |
| `/Library/LaunchDaemons/io.ironmesh.server-node.plist` | System-wide `launchd` job |
| `/Library/Application Support/Ironmesh/bin/` | Root-owned server binary and configuration launcher |
| `/Library/Application Support/Ironmesh/server-node.env` | Service-owned, mode `0600` runtime configuration |
| `/Library/Application Support/Ironmesh/server-node/` | Service-owned durable node state |
| `/Library/Logs/Ironmesh/` | `launchd` stdout and stderr logs |

## Uninstall

Run the repository helper as an administrator to stop and unregister the
daemon and remove its executable, plist, and logs. It preserves node data and
configuration by default so a reinstall or recovery does not destroy a node:

```bash
sudo ./scripts/uninstall-macos-server-node.sh
```

The `_ironmesh` non-login account is intentionally retained. Removing an
account is a machine-administration decision and is not necessary for a later
reinstall; remove it manually only after confirming that no Ironmesh service or
data still needs it.

Only use `--purge-data` when the configuration, identities, and all stored data
should be permanently deleted:

```bash
sudo ./scripts/uninstall-macos-server-node.sh --purge-data
```
