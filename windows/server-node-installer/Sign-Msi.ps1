<#
.SYNOPSIS
Authenticode-signs and verifies a BerryKeep Server Node MSI.

.DESCRIPTION
Uses a PFX only for this process, optionally timestamps the resulting MSI, and
verifies that its Authenticode signer matches the expected certificate
thumbprint.
The script intentionally does not print certificate paths, passwords, or
private-key material.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$MsiPath,
    [Parameter(Mandatory)][string]$SigningCertificatePath,
    [Parameter(Mandatory)][string]$SigningCertificatePassword,
    [string]$TimestampUrl,
    [Parameter(Mandatory)][string]$SigningCertificateThumbprint
)

$ErrorActionPreference = "Stop"

function Assert-HttpsUri {
    param([string]$Value, [string]$ParameterName)

    $uri = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -ne "https") {
        throw "$ParameterName must be an absolute HTTPS URI. Received: '$Value'."
    }
}

function Normalize-Thumbprint {
    param([string]$Value)

    return ($Value -replace '\s', '').ToUpperInvariant()
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

if (-not (Test-Path -LiteralPath $MsiPath -PathType Leaf)) {
    throw "MSI was not found: $MsiPath"
}
if (-not (Test-Path -LiteralPath $SigningCertificatePath -PathType Leaf)) {
    throw "Signing certificate was not found: $SigningCertificatePath"
}
if ($TimestampUrl) {
    Assert-HttpsUri -Value $TimestampUrl -ParameterName "-TimestampUrl"
}

$SigningCertificateThumbprint = Normalize-Thumbprint -Value $SigningCertificateThumbprint
if ($SigningCertificateThumbprint -notmatch '^[0-9A-F]{40}$') {
    throw "-SigningCertificateThumbprint must be a SHA-1 certificate thumbprint."
}

$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
    $SigningCertificatePath,
    $SigningCertificatePassword
)
if (-not $certificate.HasPrivateKey) {
    throw "Signing certificate does not contain a private key."
}
$actualThumbprint = Normalize-Thumbprint -Value $certificate.Thumbprint
if ($actualThumbprint -ne $SigningCertificateThumbprint) {
    throw "-SigningCertificateThumbprint does not match -SigningCertificatePath."
}

$signTool = Find-SignTool
if ([string]::IsNullOrWhiteSpace($signTool)) {
    throw "SignTool.exe was not found. Install the Windows SDK signing tools or add SignTool.exe to PATH."
}

$arguments = @(
    "sign",
    "/fd", "SHA256",
    "/f", $SigningCertificatePath,
    "/p", $SigningCertificatePassword
)
if ($TimestampUrl) {
    $arguments += @("/tr", $TimestampUrl, "/td", "SHA256")
}
$arguments += $MsiPath
& $signTool @arguments
if ($LASTEXITCODE -ne 0) {
    throw "SignTool failed with exit code $LASTEXITCODE"
}

& (Join-Path $PSScriptRoot '..\Verify-ExpectedAuthenticodeSignature.ps1') `
    -FilePath $MsiPath `
    -SigningCertificateThumbprint $SigningCertificateThumbprint

Write-Host "Signed and verified BerryKeep Server Node MSI:" -ForegroundColor Green
Write-Host "  $MsiPath"
