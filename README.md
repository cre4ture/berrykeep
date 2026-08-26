<p align="center">
  <img src="docs/assets/ironmesh-favicon.svg" alt="BerryKeep logo" width="144" />
</p>

# BerryKeep

Your data. Your hardware. Your storage cluster.

BerryKeep is private, self-hosted storage for files and media. It turns the
hardware you already own into a resilient storage cluster, so files, folders,
and media stay under your control without sacrificing the convenience people
expect from a cloud drive. Secure multi-node storage, offline-friendly sync and
conflict handling, and native access paths bring the same data cleanly to the
web, mobile, and operating-system file managers.

## Built for the hardware you already own

You do not need a rack, a NAS appliance, or enterprise disks to run BerryKeep
at home. Put a server node on an unused desktop or laptop, a small always-on
Linux computer such as a Raspberry Pi-class device. On those computers, an
inexpensive external USB drive is enough for each node; give it a stable mount
location and a stable place on your network. That is a practical BerryKeep
cluster. The repository also includes an Android Server Node app for compatible
spare Android phones.

The important part is the cluster, not any individual device. A straightforward
home setup uses three devices in different places in the house, each with its
own USB drive. With enough online nodes and capacity, BerryKeep's default
replication factor of three stores independent replicas across the cluster. It
places data, replicates it after writes, and repairs degraded replicas from a
healthy peer automatically. A failed drive or an unreliable node should not
turn into data loss because the remaining nodes still hold the other copies.

BerryKeep also watches the machines that hold your data. Node health reporting
automatically detects and surfaces storage-integrity problems, repair failures,
disk-health signals (when SMART is available), thermal throttling, ECC memory
errors on capable systems, and network-error indicators in the administrator
UI. You can spot unstable hardware early, replace it deliberately, and let the
cluster restore its intended redundancy instead of discovering a problem only
after a disk has failed.

This is intentionally modest hardware used well: reclaim devices that would
otherwise sit unused, start with a single external drive per node, then add
capacity or nodes when your library grows. The current supported installation
paths are documented below.

## Renaming from IronMesh

This project is being renamed from **IronMesh** to **BerryKeep** because the
former name is already used by another project. The repository and
user-facing product surfaces are now moving to the BerryKeep name and the new
grape-and-leaf icon.

The rename is deliberately incremental. To preserve compatibility with
existing installs, data, automation, and package upgrades, many technical
identifiers still use the legacy `ironmesh` name. These include Cargo package
and crate names, binary and command names, environment variables, systemd
service and user names, data directories, the APT repository and package
names, application identities, and some asset and source-file names.

No action is required from existing users: continue using the installation,
service, and configuration commands in this README exactly as written. A
future compatibility-aware migration will update the remaining identifiers;
until then, an `ironmesh` reference in a command, path, package name, or
configuration key is expected and does not identify a separate product.

## Project Status

BerryKeep is experimental software under active development. Versioned packages
and tagged releases exist so deployments and upgrades can be tested, but they
do not mean the project is production-ready yet.

- Maturity: experimental. APIs, storage details, replication behavior, and
  operational workflows may still change between releases.
- Guarantees: BerryKeep does not currently offer availability, durability,
  support, or backward-compatibility guarantees for use as a live primary
  storage system.
- Data safety: do not use BerryKeep as the only copy of important data. Keep
  independent backups and recovery procedures outside BerryKeep.
- Security reporting: report vulnerabilities privately as described in
  [SECURITY.md](SECURITY.md).
- License: MIT. See [LICENSE](LICENSE).

Current direction highlights:

- Cluster-aware storage with deterministic placement, automatic replication and repair, and a no-loss version model for offline or concurrent edits.
- Native access paths across the web UI, CLI, Android, Linux FUSE, and Windows CFAPI placeholder integration, with on-demand hydration where the platform supports it.
- Secure onboarding and connectivity through guided zero-touch cluster setup, certificate-backed identities, and rendezvous/relay paths for harder network topologies.
- [Device-scoped access to private node-local web applications](docs/private-web-services.md) through isolated loopback browser origins, with upstream CA trust or exact self-signed certificate pinning and no public home-network ingress.
- Media-aware browsing with cached thumbnails and metadata designed to support gallery-style experiences without downloading original files first.
- Hardware-health reporting that makes storage, runtime, and host reliability signals visible before a weak node becomes a bigger problem.

BerryKeep draws inspiration from [PicApport](https://www.picapport.de/de/index.php) on the self-hosted media/gallery side and [Syncthing](https://syncthing.net/) on the private, direct-first synchronization side.

## Personal motivation

As a software engineer, I want the same relationship with my computer and my data that a skilled mechanic has with a car: the ability to repair it, understand it, and extend it when needed. That desire does not come from distrust of large cloud providers, just as a mechanic's wish to work on a car does not imply suspicion of major manufacturers. It comes from knowing the craft well enough to want meaningful influence over the systems one depends on.

BerryKeep is also a test of what is now possible for an individual builder. AI coding agents have expanded the practical reach of small teams and solo engineers by an order of magnitude, and part of this project is to explore that shift seriously. Proving that this kind of ambitious, deeply owned software can be built in a new way is not separate from the project's purpose; it is one of its central goals.

## At A Glance (Legacy Diagram)

<p align="center">
  <a href="docs/assets/ironmesh-at-a-glance.png">
    <img
      src="docs/assets/ironmesh-at-a-glance.png"
      alt="IronMesh at a glance overview diagram (legacy branding)"
      width="1200"
    />
  </a>
</p>

## Install A Server Node On Windows

The native Windows x64 path is a separate MSI package for the **BerryKeep
Server Node**. It installs an automatic Windows service that starts at boot,
preserves node state under `C:\ProgramData\BerryKeep\ServerNode`, and opens
the first-run setup UI on `https://localhost:8443/`.

Build the package on a Windows machine with the .NET SDK and Rust MSVC build
prerequisites:

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\server-node-installer\Build-Msi.ps1
```

Install the resulting MSI from an elevated PowerShell, then use the setup UI to
start a cluster or join an existing one. Detailed build, signing, firewall, and
operational instructions are in
[windows/server-node-installer/README.md](windows/server-node-installer/README.md).

The Windows server package is intentionally separate from the Store/MSIX
desktop-client package: the server must run without a signed-in desktop user.

## Install On Ubuntu

BerryKeep Ubuntu packages are published from the signed APT repository at:

```text
https://creax.de/apt/ironmesh
```

These packages follow the experimental status above and are intended for
evaluation and controlled self-hosted testing.

Published package targets are:

| Ubuntu release | Architecture |
| --- | --- |
| 24.04 LTS (`noble`) | `amd64` |
| 20.04 LTS (`focal`) | `arm64` |

Use the entry matching both the Ubuntu release and the host architecture. For
example, an Ubuntu 20.04 ARM64 host uses `focal` and `arm64` below.

First install the basic apt transport/key tools:

```bash
sudo apt update
sudo apt install ca-certificates curl gnupg
sudo install -d -m 0755 /usr/share/keyrings
```

Install the legacy `ironmesh` repository signing key (the package
infrastructure is not renamed yet):

```bash
curl -fsSL https://creax.de/apt/ironmesh/ironmesh-archive-keyring.asc \
  | sudo gpg --dearmor --yes -o /usr/share/keyrings/ironmesh-archive-keyring.gpg
```

Add exactly one apt source, matching the Ubuntu release and architecture of the
host:

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

Install the server-node package:

```bash
sudo apt update
sudo apt install ironmesh-server-node
```

The package contains the generic static Server Node for the selected CPU
architecture. In particular, the `arm64` package is intended to run on 64-bit
Raspberry Pi OS as well as supported Ubuntu ARM64 hosts. A 32-bit Raspberry Pi
OS installation needs a separate future `armhf` package.

Natural Earth map conversion is optional. Install its external GDAL and unzip
tools separately through the companion package when required:

```bash
sudo apt install ironmesh-server-node-map-tools
```

## Start A Server Node

The `ironmesh-server-node` package installs a systemd service, but it does not
start it automatically. Configure the service first:

```bash
sudoedit /etc/ironmesh/server-node.env
```

For a first node in a new cluster, this minimal configuration is enough:

```bash
IRONMESH_DATA_DIR=/var/lib/ironmesh-server-node
IRONMESH_SERVER_BIND=0.0.0.0:8443
```

Then enable the service for boot and start it immediately:

```bash
sudo systemctl enable --now ironmesh-server-node.service
```

Check the service:

```bash
systemctl status ironmesh-server-node.service
journalctl -u ironmesh-server-node.service -f
```

Open the setup UI in a browser:

```text
https://<server-hostname-or-ip>:8443/
```

Accept the temporary self-signed certificate warning, choose `Start a new
cluster` on the first node, and use the setup UI to connect additional nodes.

The package creates a dedicated `ironmesh-server-node` system user. The service
runs as that user, and systemd creates `/var/lib/ironmesh-server-node` as its
state directory.

If you upgrade from an earlier beta package that ran the service as `root`, fix
existing data ownership once:

```bash
sudo chown -R ironmesh-server-node:ironmesh-server-node /var/lib/ironmesh-server-node
sudo systemctl restart ironmesh-server-node.service
```

## Install On macOS

The headless macOS server-node package installs a native `launchd`
`LaunchDaemon`, which runs at boot under a dedicated non-login service account.
It is separate from the Ubuntu `systemd` package above and does not depend on
an interactive desktop login. Build, code-sign, install, configure, operate,
and remove the package according to the
[macOS server-node package guide](docs/macos-server-node.md).

## Manual Cluster Initialisation Steps

For fresh clusters that should support the gallery map view, initialize the
self-hosted map datasets before expecting map-backed media browsing to work.
The manual administrator flow is documented in
[docs/map-viewer-data-installation.md](docs/map-viewer-data-installation.md).

## Developer Documentation

Developer-oriented workspace notes, local test commands, runtime environment
contracts, and API details live in
[docs/developer-workspace.md](docs/developer-workspace.md).
