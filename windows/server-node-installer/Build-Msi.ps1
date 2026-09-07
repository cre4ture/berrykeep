<#
.SYNOPSIS
Builds the x64 BerryKeep Server Node MSI.

.DESCRIPTION
Builds the release server-node executable, then uses the pinned WiX SDK
packages in BerryKeepServerNode.wixproj to produce a per-machine MSI. The
installer owns program files, the Windows service, and local-subnet firewall
rules. It intentionally leaves ProgramData state intact on uninstall.

When signing inputs are supplied, delegates Authenticode signing and
verification to Sign-Msi.ps1. This lets CI build an unsigned installer before
the protected signing job receives any private-key material.
#>
[CmdletBinding()]
param(
    [string]$ProductVersion = "",
    [string]$CargoTargetDir = "",
    [string]$OutputDirectory = "",
    [string]$SigningCertificatePath = "",
    [string]$SigningCertificatePassword = "",
    [string]$TimestampUrl = "",
    [string]$UpdateManifestUri = "https://github.com/cre4ture/berrykeep/releases/latest/download/berrykeep-server-node-stable.json",
    [string]$UpdateManifestSignatureUri = "https://github.com/cre4ture/berrykeep/releases/latest/download/berrykeep-server-node-stable.json.p7s",
    [string]$SigningCertificateThumbprint = "",
    [switch]$SkipCargoBuild
)

$ErrorActionPreference = "Stop"

function Get-WorkspaceVersion {
    param([string]$CargoTomlPath)

    $inWorkspacePackage = $false
    foreach ($line in Get-Content -LiteralPath $CargoTomlPath) {
        if ($line -match '^\[workspace\.package\]$') {
            $inWorkspacePackage = $true
            continue
        }
        if ($line -match '^\[') {
            $inWorkspacePackage = $false
        }
        if ($inWorkspacePackage -and $line -match '^version\s*=\s*"([^"]+)"') {
            return $Matches[1]
        }
    }

    throw "Could not read [workspace.package].version from $CargoTomlPath"
}

function Assert-MsiVersion {
    param([string]$Version)

    if ($Version -notmatch '^([1-9]\d*)\.(\d+)\.(\d+)$') {
        throw "MSI ProductVersion must be a three-part release version beginning at 1, such as 1.2.3. Received: '$Version'."
    }
    foreach ($part in $Matches[1..3]) {
        if ([int64]$part -gt 65535) {
            throw "MSI ProductVersion components cannot exceed 65535. Received: '$Version'."
        }
    }
}

function Assert-HttpsUri {
    param(
        [string]$Value,
        [string]$ParameterName
    )

    $uri = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -ne "https") {
        throw "$ParameterName must be an absolute HTTPS URI. Received: '$Value'."
    }
}

function Normalize-Thumbprint {
    param([string]$Value)

    return ($Value -replace '\s', '').ToUpperInvariant()
}

$installerRoot = $PSScriptRoot
$repoRoot = Split-Path -Parent (Split-Path -Parent $installerRoot)
$cargoTomlPath = Join-Path $repoRoot "Cargo.toml"
$projectPath = Join-Path $installerRoot "BerryKeepServerNode.wixproj"
$environmentTemplate = Join-Path $installerRoot "server-node.env"
$updateConfigurationTemplate = Join-Path $installerRoot "server-node-update.json.in"
$updateConfigurationFile = Join-Path ([System.IO.Path]::GetTempPath()) ("berrykeep-server-node-update-{0}.json" -f [Guid]::NewGuid())

if ([string]::IsNullOrWhiteSpace($ProductVersion)) {
    $ProductVersion = Get-WorkspaceVersion -CargoTomlPath $cargoTomlPath
}
Assert-MsiVersion -Version $ProductVersion

if ([string]::IsNullOrWhiteSpace($CargoTargetDir)) {
    $CargoTargetDir = Join-Path $repoRoot "target"
}
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $installerRoot ("out\BerryKeepServerNode_{0}_x64" -f $ProductVersion)
}

if (-not (Get-Command dotnet -ErrorAction SilentlyContinue)) {
    throw "dotnet SDK was not found. Install the .NET SDK required by WiX Toolset, then rerun this script."
}
if (-not (Test-Path -LiteralPath $environmentTemplate)) {
    throw "Service environment template was not found: $environmentTemplate"
}
if (-not (Test-Path -LiteralPath $updateConfigurationTemplate)) {
    throw "Server Node update configuration template was not found: $updateConfigurationTemplate"
}
Assert-HttpsUri -Value $UpdateManifestUri -ParameterName "-UpdateManifestUri"
Assert-HttpsUri -Value $UpdateManifestSignatureUri -ParameterName "-UpdateManifestSignatureUri"

$SigningCertificateThumbprint = Normalize-Thumbprint -Value $SigningCertificateThumbprint
if (-not [string]::IsNullOrWhiteSpace($SigningCertificateThumbprint) -and $SigningCertificateThumbprint -notmatch '^[0-9A-F]{40}$') {
    throw "-SigningCertificateThumbprint must be a SHA-1 certificate thumbprint. Received: '$SigningCertificateThumbprint'."
}
if (-not [string]::IsNullOrWhiteSpace($SigningCertificatePath)) {
    $signingCertificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new($SigningCertificatePath, $SigningCertificatePassword)
    $actualSigningThumbprint = Normalize-Thumbprint -Value $signingCertificate.Thumbprint
    if ([string]::IsNullOrWhiteSpace($SigningCertificateThumbprint)) {
        $SigningCertificateThumbprint = $actualSigningThumbprint
    }
    elseif ($SigningCertificateThumbprint -ne $actualSigningThumbprint) {
        throw "-SigningCertificateThumbprint does not match -SigningCertificatePath."
    }
}

$updateConfiguration = Get-Content -LiteralPath $updateConfigurationTemplate -Raw
$updateConfiguration = $updateConfiguration.Replace("__UPDATE_MANIFEST_URI__", $UpdateManifestUri)
$updateConfiguration = $updateConfiguration.Replace("__UPDATE_MANIFEST_SIGNATURE_URI__", $UpdateManifestSignatureUri)
$updateConfiguration = $updateConfiguration.Replace("__SIGNING_CERTIFICATE_THUMBPRINT__", $SigningCertificateThumbprint)
[System.IO.File]::WriteAllText($updateConfigurationFile, $updateConfiguration, [System.Text.UTF8Encoding]::new($false))

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

if (-not $SkipCargoBuild) {
    Push-Location $repoRoot
    try {
        $env:CARGO_TARGET_DIR = $CargoTargetDir
        cargo build --locked --release -p server-node --bin berrykeep-server-node
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

$serverNodeExecutable = Join-Path $CargoTargetDir "release\berrykeep-server-node.exe"
if (-not (Test-Path -LiteralPath $serverNodeExecutable)) {
    throw "Server node executable was not found: $serverNodeExecutable. Run without -SkipCargoBuild or provide -CargoTargetDir."
}

Push-Location $installerRoot
try {
    dotnet build $projectPath --configuration Release --nologo `
        "-p:ProductVersion=$ProductVersion" `
        "-p:ServerNodeExecutable=$serverNodeExecutable" `
        "-p:ServiceEnvironmentFile=$environmentTemplate" `
        "-p:UpdateConfigurationFile=$updateConfigurationFile" `
        "-p:OutputPath=$OutputDirectory\"
    if ($LASTEXITCODE -ne 0) {
        throw "WiX build failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
    Remove-Item -LiteralPath $updateConfigurationFile -Force -ErrorAction SilentlyContinue
}

$msiPath = Join-Path $OutputDirectory "BerryKeepServerNode.msi"
if (-not (Test-Path -LiteralPath $msiPath)) {
    $msiPath = Get-ChildItem -LiteralPath $OutputDirectory -Filter *.msi -File |
        Select-Object -First 1 -ExpandProperty FullName
}
if ([string]::IsNullOrWhiteSpace($msiPath) -or -not (Test-Path -LiteralPath $msiPath)) {
    throw "WiX completed but did not produce an MSI in $OutputDirectory"
}

if (-not [string]::IsNullOrWhiteSpace($SigningCertificatePath)) {
    $signMsiScript = Join-Path $installerRoot "Sign-Msi.ps1"
    if (-not (Test-Path -LiteralPath $signMsiScript)) {
        throw "MSI signing script was not found: $signMsiScript"
    }
    & $signMsiScript `
        -MsiPath $msiPath `
        -SigningCertificatePath $SigningCertificatePath `
        -SigningCertificatePassword $SigningCertificatePassword `
        -TimestampUrl $TimestampUrl `
        -SigningCertificateThumbprint $SigningCertificateThumbprint
}

Write-Host "Built BerryKeep Server Node MSI:" -ForegroundColor Green
Write-Host "  $msiPath"
if ([string]::IsNullOrWhiteSpace($SigningCertificatePath)) {
    Write-Warning "The MSI is unsigned. Release builds must pass -SigningCertificatePath and -TimestampUrl."
}
Write-Host "Install from an elevated PowerShell with:"
Write-Host "  msiexec.exe /i `"$msiPath`""
