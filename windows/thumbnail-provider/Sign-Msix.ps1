<#
.SYNOPSIS
Signs and verifies a BerryKeep MSIX package with an explicit PFX.

.DESCRIPTION
Loads the PFX only for this process, checks that its certificate matches the
MSIX manifest publisher and the expected thumbprint, optionally timestamps the
signature, verifies the signed package, and exports only the public certificate
for sideload installation. The script deliberately never creates, exports, or
logs private key material.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$MsixPath,
    [Parameter(Mandatory)][string]$SigningCertificatePath,
    [Parameter(Mandatory)][string]$SigningCertificatePassword,
    [Parameter(Mandatory)][string]$SigningCertificateThumbprint,
    [string]$TimestampUrl,
    [string]$PublicCertificatePath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

function Normalize-Thumbprint {
    param([string]$Value)

    return ($Value -replace '\s', '').ToUpperInvariant()
}

function Assert-HttpsUri {
    param([string]$Value, [string]$ParameterName)

    $uri = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -ne 'https') {
        throw "$ParameterName must be an absolute HTTPS URI. Received: '$Value'."
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
        ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
        Where-Object { Test-Path -LiteralPath $_ } |
        Select-Object -First 1
}

function Get-MsixManifestPublisher {
    param([string]$Path)

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($Path)
    try {
        $manifestEntry = $archive.Entries | Where-Object { $_.FullName -ieq 'AppxManifest.xml' } | Select-Object -First 1
        if ($null -eq $manifestEntry) {
            throw "MSIX does not contain AppxManifest.xml: $Path"
        }

        $reader = [System.IO.StreamReader]::new($manifestEntry.Open())
        try {
            [xml]$manifest = $reader.ReadToEnd()
        }
        finally {
            $reader.Dispose()
        }

        $publisher = [string]$manifest.Package.Identity.Publisher
        if ([string]::IsNullOrWhiteSpace($publisher)) {
            throw "MSIX manifest has no package publisher: $Path"
        }
        return $publisher
    }
    finally {
        $archive.Dispose()
    }
}

if (-not (Test-Path -LiteralPath $MsixPath -PathType Leaf)) {
    throw "MSIX was not found: $MsixPath"
}
if (-not (Test-Path -LiteralPath $SigningCertificatePath -PathType Leaf)) {
    throw 'The signing certificate was not found.'
}
if ($TimestampUrl) {
    Assert-HttpsUri -Value $TimestampUrl -ParameterName '-TimestampUrl'
}

$SigningCertificateThumbprint = Normalize-Thumbprint -Value $SigningCertificateThumbprint
if ($SigningCertificateThumbprint -notmatch '^[0-9A-F]{40}$') {
    throw '-SigningCertificateThumbprint must be a SHA-1 certificate thumbprint.'
}

$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
    $SigningCertificatePath,
    $SigningCertificatePassword
)
if (-not $certificate.HasPrivateKey) {
    throw 'Signing certificate does not contain a private key.'
}
if ((Normalize-Thumbprint -Value $certificate.Thumbprint) -ne $SigningCertificateThumbprint) {
    throw '-SigningCertificateThumbprint does not match -SigningCertificatePath.'
}

$manifestPublisher = Get-MsixManifestPublisher -Path $MsixPath
if ($certificate.Subject -cne $manifestPublisher) {
    throw "The signing certificate subject does not match the MSIX manifest publisher. Expected '$manifestPublisher'."
}

$signTool = Find-SignTool
if ([string]::IsNullOrWhiteSpace($signTool)) {
    throw 'SignTool.exe was not found. Install the Windows SDK signing tools or add SignTool.exe to PATH.'
}

$arguments = @(
    'sign',
    '/fd', 'SHA256',
    '/f', $SigningCertificatePath,
    '/p', $SigningCertificatePassword
)
if ($TimestampUrl) {
    $arguments += @('/tr', $TimestampUrl, '/td', 'SHA256')
}
$arguments += $MsixPath

& $signTool @arguments
if ($LASTEXITCODE -ne 0) {
    throw "SignTool failed with exit code $LASTEXITCODE"
}

$signature = Get-AuthenticodeSignature -LiteralPath $MsixPath
if ($null -eq $signature.SignerCertificate) {
    throw "MSIX does not have an Authenticode signer. Status: $($signature.Status)."
}
if ((Normalize-Thumbprint -Value $signature.SignerCertificate.Thumbprint) -ne $SigningCertificateThumbprint) {
    throw 'MSIX Authenticode signer does not match the expected certificate.'
}
if ($signature.Status -notin @('Valid', 'NotTrusted')) {
    throw "MSIX signature validation failed. Status: $($signature.Status)."
}
if ($signature.Status -eq 'NotTrusted') {
    Write-Host 'Accepted the expected self-signed MSIX signer without modifying certificate stores.'
}

if ($PublicCertificatePath) {
    $publicCertificateDirectory = Split-Path -Parent $PublicCertificatePath
    if (-not [string]::IsNullOrWhiteSpace($publicCertificateDirectory)) {
        New-Item -ItemType Directory -Path $publicCertificateDirectory -Force | Out-Null
    }
    [System.IO.File]::WriteAllBytes(
        $PublicCertificatePath,
        $certificate.Export([Security.Cryptography.X509Certificates.X509ContentType]::Cert)
    )
}

Write-Host 'Signed and verified BerryKeep MSIX:' -ForegroundColor Green
Write-Host "  $MsixPath"
