# BerryKeep Server Node MSI

This directory contains the native Windows x64 installer for the BerryKeep
Server Node. It is intentionally separate from the Store/MSIX desktop-client
package: a storage node must start at boot and continue running without a
signed-in desktop user.

The MSI installs `ironmesh-server-node.exe` as the `BerryKeepServerNode`
Windows service. The legacy executable name remains part of the compatibility
contract while the product name is BerryKeep.

## What the MSI manages

- `C:\Program Files\BerryKeep\Server Node\ironmesh-server-node.exe`
- an automatic `NT AUTHORITY\LocalService` Windows service named
  `BerryKeepServerNode`
- service recovery: restart after each of the first three failures, after five
  seconds
- local-subnet inbound firewall rules for TCP `8443` (setup/client access),
  `18443` (cluster transport), and `9443` (managed rendezvous)
- a protected service configuration and data root at
  `C:\ProgramData\BerryKeep\ServerNode`

The data root is intentionally permanent: uninstalling or upgrading the MSI
does not remove node state, enrollment material, or user configuration.

## Build

On Windows, install a current .NET SDK and the Rust MSVC build prerequisites.
The WiX Toolset SDK and its extensions are restored at the pinned version by
the project file.

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\server-node-installer\Build-Msi.ps1
```

The MSI is written beneath `windows/server-node-installer/out/`. To package an
already-built binary from a nondefault Cargo target directory:

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\server-node-installer\Build-Msi.ps1 `
  -SkipCargoBuild `
  -CargoTargetDir C:\build\ironmesh-target `
  -ProductVersion 1.0.38
```

The default output is intentionally unsigned for local development. The release
pipeline must sign it with the product code-signing certificate and timestamp
it; the build helper supports that without placing certificate material in the
repository:

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\server-node-installer\Build-Msi.ps1 `
  -SigningCertificatePath C:\secure\berrykeep-release.pfx `
  -SigningCertificatePassword $env:BERRYKEEP_SIGNING_PASSWORD `
  -TimestampUrl https://<approved-rfc3161-timestamp-service>
```

`Sign-Msi.ps1` signs an already-built MSI and verifies its signer. Release CI
uses it in a separate protected job after the unsigned build artifact is
available, so the build itself never receives private-key material:

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\server-node-installer\Sign-Msi.ps1 `
  -MsiPath .\windows\server-node-installer\out\BerryKeepServerNode_1.0.38_x64\BerryKeepServerNode.msi `
  -SigningCertificatePath C:\secure\berrykeep-release.pfx `
  -SigningCertificatePassword $env:BERRYKEEP_SIGNING_PASSWORD `
  -TimestampUrl https://<approved-rfc3161-timestamp-service> `
  -SigningCertificateThumbprint <certificate-thumbprint>
```

## Automatic updates

Release MSIs embed a pinned release-signing certificate thumbprint and an
initially disabled stable-release manifest URL. An administrator can opt in to
a daily `SYSTEM` scheduled task that downloads only a CMS-signed manifest and
a matching Authenticode-signed MSI during a local maintenance window:

```powershell
& "${env:ProgramFiles}\BerryKeep\Server Node\Enable-ServerNodeAutoUpdate.ps1" `
  -MaintenanceWindowStart "03:00" `
  -MaintenanceWindowEnd "05:00"
```

Disable it with:

```powershell
& "${env:ProgramFiles}\BerryKeep\Server Node\Disable-ServerNodeAutoUpdate.ps1"
```

See [Automatic update operations](../../docs/windows-server-node-auto-update.md)
for the signer checks, release assets, and failure handling.

## Install and first-run setup

Run the generated package from an elevated PowerShell:

```powershell
msiexec.exe /i .\BerryKeepServerNode.msi
```

The MSI starts the service automatically. From the node itself, open
`https://localhost:8443/`; from another computer on the same private or domain
network, open `https://<windows-hostname-or-ip>:8443/`. Accept the temporary
self-signed certificate, then choose either **Start a new cluster** or **Join
an existing cluster**.

The first-run listener is intentionally local-subnet scoped. Publishing a node
through a reverse proxy or to an Internet-facing network is an advanced
operator deployment: use a trusted certificate, restrict remote firewall
access deliberately, and keep an independent backup of the state directory.

## Operation

```powershell
Get-Service BerryKeepServerNode
Restart-Service BerryKeepServerNode
Get-Content C:\ProgramData\BerryKeep\ServerNode\server-node.env
```

`server-node.env` supplies only installation-local start values. Restart the
service after editing it. Retain the supplied `BERRYKEEP_SERVER_NODE_*`
variables for the guided setup mode; adding ordinary `IRONMESH_*` runtime
variables selects the advanced environment-driven startup path instead.

To remove the program and service while preserving data:

```powershell
msiexec.exe /x .\BerryKeepServerNode.msi
```

Delete `C:\ProgramData\BerryKeep\ServerNode` only after an independently
verified backup and when the node is permanently retired.
