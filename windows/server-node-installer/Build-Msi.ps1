<#
.SYNOPSIS
Builds the x64 BerryKeep Server Node MSI.

.DESCRIPTION
Builds the release server-node executable, then uses the pinned WiX SDK
packages in BerryKeepServerNode.wixproj to produce a per-machine MSI. The
installer owns program files, the Windows service, and local-subnet firewall
rules. It intentionally leaves ProgramData state intact on uninstall.
#>
[CmdletBinding()]
param(
    [string]$ProductVersion = "",
    [string]$CargoTargetDir = "",
    [string]$OutputDirectory = "",
    [string]$SigningCertificatePath = "",
    [string]$SigningCertificatePassword = "",
    [string]$TimestampUrl = "",
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

function Find-SignTool {
    $command = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $kitsRoot = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
    if (-not (Test-Path -LiteralPath $kitsRoot)) {
        return $null
    }

    return Get-ChildItem -LiteralPath $kitsRoot -Directory |
        Sort-Object Name -Descending |
        ForEach-Object { Join-Path $_.FullName "x64\signtool.exe" } |
        Where-Object { Test-Path -LiteralPath $_ } |
        Select-Object -First 1
}

$installerRoot = $PSScriptRoot
$repoRoot = Split-Path -Parent (Split-Path -Parent $installerRoot)
$cargoTomlPath = Join-Path $repoRoot "Cargo.toml"
$projectPath = Join-Path $installerRoot "BerryKeepServerNode.wixproj"
$environmentTemplate = Join-Path $installerRoot "server-node.env"

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

New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null

if (-not $SkipCargoBuild) {
    Push-Location $repoRoot
    try {
        $env:CARGO_TARGET_DIR = $CargoTargetDir
        cargo build --locked --release -p server-node --bin ironmesh-server-node
        if ($LASTEXITCODE -ne 0) {
            throw "cargo build failed with exit code $LASTEXITCODE"
        }
    }
    finally {
        Pop-Location
    }
}

$serverNodeExecutable = Join-Path $CargoTargetDir "release\ironmesh-server-node.exe"
if (-not (Test-Path -LiteralPath $serverNodeExecutable)) {
    throw "Server node executable was not found: $serverNodeExecutable. Run without -SkipCargoBuild or provide -CargoTargetDir."
}

Push-Location $installerRoot
try {
    dotnet build $projectPath --configuration Release --nologo `
        "-p:ProductVersion=$ProductVersion" `
        "-p:ServerNodeExecutable=$serverNodeExecutable" `
        "-p:ServiceEnvironmentFile=$environmentTemplate" `
        "-p:OutputPath=$OutputDirectory\"
    if ($LASTEXITCODE -ne 0) {
        throw "WiX build failed with exit code $LASTEXITCODE"
    }
}
finally {
    Pop-Location
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
    if ([string]::IsNullOrWhiteSpace($TimestampUrl)) {
        throw "-TimestampUrl is required when -SigningCertificatePath is supplied."
    }
    if (-not (Test-Path -LiteralPath $SigningCertificatePath)) {
        throw "Signing certificate was not found: $SigningCertificatePath"
    }

    $signTool = Find-SignTool
    if ([string]::IsNullOrWhiteSpace($signTool)) {
        throw "SignTool.exe was not found. Install the Windows SDK signing tools or add SignTool.exe to PATH."
    }

    $arguments = @("sign", "/fd", "SHA256", "/f", $SigningCertificatePath)
    if (-not [string]::IsNullOrWhiteSpace($SigningCertificatePassword)) {
        $arguments += @("/p", $SigningCertificatePassword)
    }
    $arguments += @("/tr", $TimestampUrl, "/td", "SHA256", $msiPath)
    & $signTool @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "SignTool failed with exit code $LASTEXITCODE"
    }
}

Write-Host "Built BerryKeep Server Node MSI:" -ForegroundColor Green
Write-Host "  $msiPath"
if ([string]::IsNullOrWhiteSpace($SigningCertificatePath)) {
    Write-Warning "The MSI is unsigned. Release builds must pass -SigningCertificatePath and -TimestampUrl."
}
Write-Host "Install from an elevated PowerShell with:"
Write-Host "  msiexec.exe /i `"$msiPath`""
