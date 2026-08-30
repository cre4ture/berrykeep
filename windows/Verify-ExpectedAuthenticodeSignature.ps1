<#
.SYNOPSIS
Verifies an Authenticode-signed file against an expected signer certificate.

.DESCRIPTION
Requires SignTool to validate the file signature and PowerShell to expose the
signer certificate. A self-signed test certificate is accepted only when the
certificate thumbprint matches exactly and SignTool reports the specific
untrusted-root condition. The script does not alter certificate stores.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$FilePath,
    [Parameter(Mandatory)][string]$SigningCertificateThumbprint
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

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
        ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
        Where-Object { Test-Path -LiteralPath $_ } |
        Select-Object -First 1
}

if (-not (Test-Path -LiteralPath $FilePath -PathType Leaf)) {
    throw "Signed file was not found: $FilePath"
}

$SigningCertificateThumbprint = Normalize-Thumbprint -Value $SigningCertificateThumbprint
if ($SigningCertificateThumbprint -notmatch '^[0-9A-F]{40}$') {
    throw '-SigningCertificateThumbprint must be a SHA-1 certificate thumbprint.'
}

$signTool = Find-SignTool
if ([string]::IsNullOrWhiteSpace($signTool)) {
    throw 'SignTool.exe was not found. Install the Windows SDK signing tools or add SignTool.exe to PATH.'
}

$verificationOutput = @(& $signTool 'verify' '/pa' '/v' $FilePath 2>&1)
$verificationExitCode = $LASTEXITCODE
$verificationText = ($verificationOutput | Out-String)
$hasExpectedUntrustedRoot = $verificationExitCode -ne 0 -and $verificationText -match '(?is)(0x800B0109|CERT_E_UNTRUSTEDROOT|root\s+certificate\s+which\s+is\s+not\s+trusted)'
if ($verificationExitCode -ne 0 -and -not $hasExpectedUntrustedRoot) {
    Write-Host $verificationText.TrimEnd()
    throw "SignTool verification failed with exit code $verificationExitCode."
}

$signature = Get-AuthenticodeSignature -LiteralPath $FilePath
if ($null -eq $signature.SignerCertificate) {
    throw "Signed file has no Authenticode signer. Status: $($signature.Status)."
}
if ((Normalize-Thumbprint -Value $signature.SignerCertificate.Thumbprint) -ne $SigningCertificateThumbprint) {
    throw 'Authenticode signer does not match the expected certificate.'
}

if ($verificationExitCode -eq 0) {
    if ($signature.Status -ne 'Valid') {
        throw "Authenticode signature validation failed. Status: $($signature.Status)."
    }
}
else {
    if ($signature.Status -notin @('NotTrusted', 'UnknownError')) {
        throw "Unexpected self-signed Authenticode status: $($signature.Status)."
    }
    Write-Host 'Accepted the expected self-signed signer without modifying certificate stores.'
}
