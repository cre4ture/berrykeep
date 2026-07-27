Set-StrictMode -Version Latest
Add-Type -AssemblyName System.Security

$script:UpdateTaskName = "ServerNodeUpdate"
$script:UpdateTaskPath = "\BerryKeep\"
$script:ServerNodeServiceName = "BerryKeepServerNode"

function Assert-ServerNodeAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this command from an elevated PowerShell session."
    }
}

function ConvertTo-ServerNodeThumbprint {
    param(
        [Parameter(Mandatory)]
        [AllowEmptyString()]
        [string]$Thumbprint
    )

    return ($Thumbprint -replace '\s', '').ToUpperInvariant()
}

function Assert-ServerNodeHttpsUri {
    param(
        [Parameter(Mandatory)][string]$Value,
        [Parameter(Mandatory)][string]$Name
    )

    $uri = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -ne "https") {
        throw "$Name must be an absolute HTTPS URI. Received: '$Value'."
    }

    return $uri
}

function Get-ServerNodeUpdateConfigurationPath {
    return Join-Path $env:ProgramData "BerryKeep\ServerNode\server-node-update.json"
}

function Get-ServerNodeUpdateWorkDirectory {
    $path = Join-Path $env:ProgramData "BerryKeep\ServerNode\update"
    New-Item -ItemType Directory -Path $path -Force | Out-Null
    return $path
}

function Write-ServerNodeUpdateLog {
    param(
        [Parameter(Mandatory)][string]$Message,
        [ValidateSet("INFO", "WARN", "ERROR")][string]$Level = "INFO"
    )

    $directory = Get-ServerNodeUpdateWorkDirectory
    $path = Join-Path $directory "update.log"
    $line = "{0} [{1}] {2}" -f [DateTime]::UtcNow.ToString("o"), $Level, $Message
    Add-Content -LiteralPath $path -Value $line -Encoding UTF8
}

function ConvertTo-ServerNodeTimeOfDay {
    param(
        [Parameter(Mandatory)][string]$Value,
        [Parameter(Mandatory)][string]$Name
    )

    $result = [DateTime]::MinValue
    if (-not [DateTime]::TryParseExact($Value, "HH:mm", [Globalization.CultureInfo]::InvariantCulture, [Globalization.DateTimeStyles]::None, [ref]$result)) {
        throw "$Name must use the HH:mm 24-hour format. Received: '$Value'."
    }

    return $result.TimeOfDay
}

function Get-ServerNodeUpdateConfiguration {
    param([string]$Path = (Get-ServerNodeUpdateConfigurationPath))

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "Server Node update configuration was not found: $Path"
    }

    try {
        $configuration = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
    }
    catch {
        throw "Server Node update configuration is not valid JSON: $Path. $($_.Exception.Message)"
    }

    if ($configuration.schemaVersion -ne 1) {
        throw "Unsupported Server Node update configuration schema version '$($configuration.schemaVersion)'."
    }
    if ($null -eq $configuration.enabled) {
        throw "Server Node update configuration must define 'enabled'."
    }
    if ($null -eq $configuration.manifestUri -or $null -eq $configuration.signatureUri -or $null -eq $configuration.signerThumbprint) {
        throw "Server Node update configuration is missing a release manifest setting."
    }
    if ($null -eq $configuration.maintenanceWindow -or $null -eq $configuration.maintenanceWindow.startLocal -or $null -eq $configuration.maintenanceWindow.endLocal) {
        throw "Server Node update configuration is missing its maintenance window."
    }

    $null = Assert-ServerNodeHttpsUri -Value ([string]$configuration.manifestUri) -Name "manifestUri"
    $null = Assert-ServerNodeHttpsUri -Value ([string]$configuration.signatureUri) -Name "signatureUri"
    $null = ConvertTo-ServerNodeTimeOfDay -Value ([string]$configuration.maintenanceWindow.startLocal) -Name "maintenanceWindow.startLocal"
    $null = ConvertTo-ServerNodeTimeOfDay -Value ([string]$configuration.maintenanceWindow.endLocal) -Name "maintenanceWindow.endLocal"

    $configuration.signerThumbprint = ConvertTo-ServerNodeThumbprint -Thumbprint ([string]$configuration.signerThumbprint)
    if ([bool]$configuration.enabled -and $configuration.signerThumbprint -notmatch '^[0-9A-F]{40}$') {
        throw "Enabled Server Node auto-updates require a SHA-1 signing certificate thumbprint."
    }

    return $configuration
}

function Save-ServerNodeUpdateConfiguration {
    param(
        [Parameter(Mandatory)]$Configuration,
        [string]$Path = (Get-ServerNodeUpdateConfigurationPath)
    )

    $directory = Split-Path -Parent $Path
    New-Item -ItemType Directory -Path $directory -Force | Out-Null
    $temporaryPath = Join-Path $directory ("server-node-update-{0}.tmp" -f [Guid]::NewGuid())
    try {
        [System.IO.File]::WriteAllText($temporaryPath, ($Configuration | ConvertTo-Json -Depth 5), [System.Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $temporaryPath -Destination $Path -Force
    }
    finally {
        Remove-Item -LiteralPath $temporaryPath -Force -ErrorAction SilentlyContinue
    }
}

function Test-ServerNodeMaintenanceWindow {
    param(
        [Parameter(Mandatory)]$Configuration,
        [DateTime]$Now = (Get-Date)
    )

    $start = ConvertTo-ServerNodeTimeOfDay -Value ([string]$Configuration.maintenanceWindow.startLocal) -Name "maintenanceWindow.startLocal"
    $end = ConvertTo-ServerNodeTimeOfDay -Value ([string]$Configuration.maintenanceWindow.endLocal) -Name "maintenanceWindow.endLocal"
    if ($start -eq $end) {
        throw "The Server Node maintenance window must not have identical start and end times."
    }

    $time = $Now.TimeOfDay
    if ($start -lt $end) {
        return $time -ge $start -and $time -lt $end
    }

    return $time -ge $start -or $time -lt $end
}

function Invoke-ServerNodeDownload {
    param(
        [Parameter(Mandatory)][Uri]$Uri,
        [Parameter(Mandatory)][string]$Path
    )

    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $Uri -OutFile $Path -UseBasicParsing -ErrorAction Stop
}

function Test-ServerNodeManifestSignature {
    param(
        [Parameter(Mandatory)][string]$ManifestPath,
        [Parameter(Mandatory)][string]$SignaturePath,
        [Parameter(Mandatory)][string]$ExpectedThumbprint
    )

    $expected = ConvertTo-ServerNodeThumbprint -Thumbprint $ExpectedThumbprint
    $content = [System.IO.File]::ReadAllBytes($ManifestPath)
    $signature = [System.IO.File]::ReadAllBytes($SignaturePath)
    $cms = [System.Security.Cryptography.Pkcs.SignedCms]::new([System.Security.Cryptography.Pkcs.ContentInfo]::new($content), $true)
    $cms.Decode($signature)
    $cms.CheckSignature($true)

    if ($cms.SignerInfos.Count -ne 1 -or $null -eq $cms.SignerInfos[0].Certificate) {
        throw "Release manifest must have exactly one embedded signing certificate."
    }

    $actual = ConvertTo-ServerNodeThumbprint -Thumbprint $cms.SignerInfos[0].Certificate.Thumbprint
    if ($actual -ne $expected) {
        throw "Release manifest signer '$actual' does not match the configured signer '$expected'."
    }
}

function Get-ServerNodeReleaseManifest {
    param(
        [Parameter(Mandatory)]$Configuration,
        [Parameter(Mandatory)][string]$Directory
    )

    $manifestPath = Join-Path $Directory "release-manifest.json"
    $signaturePath = Join-Path $Directory "release-manifest.json.p7s"
    Invoke-ServerNodeDownload -Uri (Assert-ServerNodeHttpsUri -Value ([string]$Configuration.manifestUri) -Name "manifestUri") -Path $manifestPath
    Invoke-ServerNodeDownload -Uri (Assert-ServerNodeHttpsUri -Value ([string]$Configuration.signatureUri) -Name "signatureUri") -Path $signaturePath
    Test-ServerNodeManifestSignature -ManifestPath $manifestPath -SignaturePath $signaturePath -ExpectedThumbprint ([string]$Configuration.signerThumbprint)

    try {
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    }
    catch {
        throw "Signed release manifest is not valid JSON. $($_.Exception.Message)"
    }

    if ($manifest.schemaVersion -ne 1 -or $manifest.channel -ne "stable") {
        throw "Signed release manifest has an unsupported schema or channel."
    }
    if ([string]$manifest.version -notmatch '^[1-9]\d*\.\d+\.\d+$') {
        throw "Signed release manifest has an invalid version '$($manifest.version)'."
    }
    if ($null -eq $manifest.installer -or [string]$manifest.installer.url -eq "" -or [string]$manifest.installer.sha256 -notmatch '^[0-9A-Fa-f]{64}$') {
        throw "Signed release manifest does not describe a valid MSI installer."
    }
    if ((ConvertTo-ServerNodeThumbprint -Thumbprint ([string]$manifest.installer.authenticodeSignerThumbprint)) -ne ([string]$Configuration.signerThumbprint)) {
        throw "Signed release manifest does not require the configured MSI signer."
    }

    $manifest.installer.url = (Assert-ServerNodeHttpsUri -Value ([string]$manifest.installer.url) -Name "installer.url").AbsoluteUri
    $manifest.installer.sha256 = ([string]$manifest.installer.sha256).ToUpperInvariant()
    return $manifest
}

function Get-InstalledServerNodeVersion {
    $locations = @(
        "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
        "HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall"
    )
    $versions = foreach ($location in $locations) {
        Get-ChildItem -LiteralPath $location -ErrorAction SilentlyContinue | ForEach-Object {
            $entry = Get-ItemProperty -LiteralPath $_.PSPath -ErrorAction SilentlyContinue
            if ($entry.DisplayName -eq "BerryKeep Server Node") {
                try {
                    [version]$entry.DisplayVersion
                }
                catch {
                    Write-ServerNodeUpdateLog -Level "WARN" -Message "Ignoring unparsable installed Server Node version '$($entry.DisplayVersion)'."
                }
            }
        }
    }

    return $versions | Sort-Object -Descending | Select-Object -First 1
}

function Test-ServerNodeInstaller {
    param(
        [Parameter(Mandatory)][string]$Path,
        [Parameter(Mandatory)]$Manifest,
        [Parameter(Mandatory)][string]$ExpectedThumbprint
    )

    $actualHash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($actualHash -ne [string]$Manifest.installer.sha256) {
        throw "Downloaded MSI SHA-256 does not match the signed release manifest."
    }

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne "Valid" -or $null -eq $signature.SignerCertificate) {
        throw "Downloaded MSI does not have a valid Authenticode signature. Status: $($signature.Status)."
    }

    $actualThumbprint = ConvertTo-ServerNodeThumbprint -Thumbprint $signature.SignerCertificate.Thumbprint
    $expected = ConvertTo-ServerNodeThumbprint -Thumbprint $ExpectedThumbprint
    if ($actualThumbprint -ne $expected) {
        throw "Downloaded MSI signer '$actualThumbprint' does not match the configured signer '$expected'."
    }
}

function Start-ServerNodeServiceAndWait {
    $service = Get-Service -Name $script:ServerNodeServiceName -ErrorAction Stop
    if ($service.Status -ne "Running") {
        Start-Service -Name $script:ServerNodeServiceName -ErrorAction Stop
    }

    $deadline = (Get-Date).AddMinutes(2)
    do {
        $service = Get-Service -Name $script:ServerNodeServiceName -ErrorAction Stop
        if ($service.Status -eq "Running") {
            return
        }
        Start-Sleep -Seconds 2
    } while ((Get-Date) -lt $deadline)

    throw "BerryKeep Server Node did not reach the Running service state after its update."
}

function Invoke-ServerNodeUpdate {
    param(
        [string]$ConfigurationPath = (Get-ServerNodeUpdateConfigurationPath),
        [switch]$Force,
        [switch]$DryRun
    )

    $configuration = Get-ServerNodeUpdateConfiguration -Path $ConfigurationPath
    if (-not [bool]$configuration.enabled -and -not $Force) {
        Write-ServerNodeUpdateLog -Message "Automatic update check skipped because it is disabled by the administrator."
        return [pscustomobject]@{ Outcome = "Skipped"; Reason = "Disabled" }
    }
    if (-not $Force -and -not (Test-ServerNodeMaintenanceWindow -Configuration $configuration)) {
        Write-ServerNodeUpdateLog -Message "Automatic update check skipped outside the configured maintenance window."
        return [pscustomobject]@{ Outcome = "Skipped"; Reason = "OutsideMaintenanceWindow" }
    }

    $workRoot = Get-ServerNodeUpdateWorkDirectory
    $runDirectory = Join-Path $workRoot ("run-{0}" -f [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
    $serviceWasRunning = $false

    try {
        $manifest = Get-ServerNodeReleaseManifest -Configuration $configuration -Directory $runDirectory
        $installedVersion = Get-InstalledServerNodeVersion
        $availableVersion = [version]$manifest.version
        if ($null -eq $installedVersion) {
            throw "BerryKeep Server Node is not registered as an installed MSI product."
        }
        if ($availableVersion -le $installedVersion) {
            Write-ServerNodeUpdateLog -Message "Installed Server Node version $installedVersion is current (available: $availableVersion)."
            return [pscustomobject]@{ Outcome = "Skipped"; Reason = "Current"; InstalledVersion = $installedVersion; AvailableVersion = $availableVersion }
        }

        $installerPath = Join-Path $runDirectory ("berrykeep-server-node-{0}-windows-x64.msi" -f $manifest.version)
        Invoke-ServerNodeDownload -Uri ([Uri]$manifest.installer.url) -Path $installerPath
        Test-ServerNodeInstaller -Path $installerPath -Manifest $manifest -ExpectedThumbprint ([string]$configuration.signerThumbprint)

        if ($DryRun) {
            Write-ServerNodeUpdateLog -Message "Validated Server Node update $installedVersion -> $availableVersion without installing it."
            return [pscustomobject]@{ Outcome = "Validated"; InstalledVersion = $installedVersion; AvailableVersion = $availableVersion }
        }

        $service = Get-Service -Name $script:ServerNodeServiceName -ErrorAction Stop
        $serviceWasRunning = $service.Status -eq "Running"
        if ($serviceWasRunning) {
            Stop-Service -Name $script:ServerNodeServiceName -Force -ErrorAction Stop
            $service.WaitForStatus("Stopped", [TimeSpan]::FromMinutes(2))
        }

        $arguments = "/i `"$installerPath`" /qn /norestart REBOOT=ReallySuppress"
        $installer = Start-Process -FilePath "msiexec.exe" -ArgumentList $arguments -Wait -PassThru -ErrorAction Stop
        if ($installer.ExitCode -notin 0, 3010) {
            throw "MSI update failed with exit code $($installer.ExitCode)."
        }

        if ($serviceWasRunning) {
            Start-ServerNodeServiceAndWait
        }
        else {
            $updatedService = Get-Service -Name $script:ServerNodeServiceName -ErrorAction Stop
            if ($updatedService.Status -ne "Stopped") {
                Stop-Service -Name $script:ServerNodeServiceName -Force -ErrorAction Stop
                $updatedService.WaitForStatus("Stopped", [TimeSpan]::FromMinutes(2))
            }
        }
        Write-ServerNodeUpdateLog -Message "Updated BerryKeep Server Node from $installedVersion to $availableVersion."
        return [pscustomobject]@{ Outcome = "Updated"; InstalledVersion = $installedVersion; AvailableVersion = $availableVersion; RebootRequired = ($installer.ExitCode -eq 3010) }
    }
    catch {
        Write-ServerNodeUpdateLog -Level "ERROR" -Message "Server Node update failed: $($_.Exception.Message)"
        if ($serviceWasRunning) {
            try {
                Start-ServerNodeServiceAndWait
            }
            catch {
                Write-ServerNodeUpdateLog -Level "ERROR" -Message "Could not restore the Server Node service after a failed update: $($_.Exception.Message)"
            }
        }
        throw
    }
    finally {
        Remove-Item -LiteralPath $runDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Export-ModuleMember -Function @(
    "Assert-ServerNodeAdministrator",
    "Assert-ServerNodeHttpsUri",
    "ConvertTo-ServerNodeThumbprint",
    "Get-ServerNodeUpdateConfiguration",
    "Get-ServerNodeUpdateConfigurationPath",
    "Get-ServerNodeUpdateWorkDirectory",
    "Get-InstalledServerNodeVersion",
    "Invoke-ServerNodeUpdate",
    "Save-ServerNodeUpdateConfiguration",
    "Test-ServerNodeMaintenanceWindow",
    "Write-ServerNodeUpdateLog"
)
