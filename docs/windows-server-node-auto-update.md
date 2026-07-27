# Windows Server Node Automatic Updates

The Windows Server Node is distributed as a signed MSI. Its automatic updater
is deliberately separate from the node service: a scheduled task running as
`SYSTEM` downloads, verifies, and installs one newer MSI during an
administrator-defined maintenance window.

## Security model

The updater accepts an update only when all of the following checks succeed:

1. The manifest and detached `.p7s` signature were fetched over HTTPS.
2. The CMS manifest signature has the certificate thumbprint pinned by the
   installed, signed MSI.
3. The manifest contains a valid stable three-part version and the SHA-256 of
   the MSI.
4. The downloaded MSI matches that SHA-256 and has a valid Authenticode
   signature from the same pinned certificate.

The updater fails closed. It does not install unsigned packages, accept a
different signer, or update outside the configured maintenance window.

## Enable or disable

Automatic updates are opt-in. After installing a signed release MSI, open an
elevated PowerShell prompt and run:

```powershell
& "${env:ProgramFiles}\BerryKeep\Server Node\Enable-ServerNodeAutoUpdate.ps1" `
  -MaintenanceWindowStart "03:00" `
  -MaintenanceWindowEnd "05:00"
```

This creates `\BerryKeep\ServerNodeUpdate`, a daily task that runs as
`SYSTEM`. The task downloads a new stable release only during the selected
local-time window. It stops `BerryKeepServerNode`, runs the MSI without a
reboot, starts the service again, and waits for the service to become running.

To turn it off:

```powershell
& "${env:ProgramFiles}\BerryKeep\Server Node\Disable-ServerNodeAutoUpdate.ps1"
```

The task removes itself if the Server Node installer has been removed. The
configuration and update log remain in `C:\ProgramData\BerryKeep\ServerNode`
alongside the intentionally persistent node data.

## Operator controls

The installed configuration file is
`C:\ProgramData\BerryKeep\ServerNode\server-node-update.json`. Administrators
may change the HTTPS manifest and signature URLs or the maintenance window.
Keep the signer thumbprint unchanged unless performing a deliberate signing-key
rotation. A certificate rotation requires a bridge release signed by the
currently trusted certificate and an explicit configuration migration.

Run a signed manifest/MSI verification without installing it:

```powershell
& "${env:ProgramFiles}\BerryKeep\Server Node\Update-ServerNode.ps1" -Force -DryRun
```

The updater writes UTC log entries to
`C:\ProgramData\BerryKeep\ServerNode\update\update.log`.

## Release publication

An annotated stable tag such as `v1.0.39` triggers `.github/workflows/release.yml`.
The protected `release-signing` environment supplies the signing certificate,
password, and RFC-3161 timestamp endpoint. The workflow publishes:

- `berrykeep-server-node-<version>-windows-x64.msi`
- `berrykeep-server-node-stable.json`
- `berrykeep-server-node-stable.json.p7s`
- `SHA256SUMS`

The stable manifest points at the versioned MSI on the GitHub Release page.
The updater reads it from the stable GitHub Release download URL.
