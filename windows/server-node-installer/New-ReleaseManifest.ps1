<#
.SYNOPSIS
Creates a signed BerryKeep Server Node stable-release manifest for the MSI updater.

.DESCRIPTION
The JSON manifest is signed as a detached CMS/PKCS#7 signature with the same
certificate that Authenticode-signs the MSI. The updater pins that certificate
thumbprint before it accepts either the manifest or the MSI.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$MsiPath,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$InstallerUri,
    [Parameter(Mandatory)][string]$SigningCertificatePath,
    [Parameter(Mandatory)][string]$SigningCertificatePassword,
    [Parameter(Mandatory)][string]$OutputDirectory
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Security

function Assert-HttpsUri {
    param([string]$Value, [string]$Name)

    $uri = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri) -or $uri.Scheme -ne "https") {
        throw "$Name must be an absolute HTTPS URI. Received: '$Value'."
    }
    return $uri
}

function ConvertTo-Thumbprint {
    param([string]$Value)

    return ($Value -replace '\s', '').ToUpperInvariant()
}

if ($Version -notmatch '^[1-9]\d*\.\d+\.\d+$') {
    throw "Version must be a three-part release version. Received: '$Version'."
}
if (-not (Test-Path -LiteralPath $MsiPath -PathType Leaf)) {
    throw "MSI was not found: $MsiPath"
}
$installerUri = Assert-HttpsUri -Value $InstallerUri -Name "-InstallerUri"
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null

$certificate = [Security.Cryptography.X509Certificates.X509Certificate2]::new(
    $SigningCertificatePath,
    $SigningCertificatePassword,
    [Security.Cryptography.X509Certificates.X509KeyStorageFlags]::Exportable
)
if (-not $certificate.HasPrivateKey) {
    throw "Signing certificate does not contain a private key."
}
$thumbprint = ConvertTo-Thumbprint -Value $certificate.Thumbprint
if ($thumbprint -notmatch '^[0-9A-F]{40}$') {
    throw "Signing certificate did not expose a usable SHA-1 thumbprint."
}

& (Join-Path $PSScriptRoot '..\Verify-ExpectedAuthenticodeSignature.ps1') `
    -FilePath $MsiPath `
    -SigningCertificateThumbprint $thumbprint

$manifest = [ordered]@{
    schemaVersion = 1
    channel = "stable"
    version = $Version
    publishedAtUtc = [DateTime]::UtcNow.ToString("o")
    installer = [ordered]@{
        url = $installerUri.AbsoluteUri
        sha256 = (Get-FileHash -LiteralPath $MsiPath -Algorithm SHA256).Hash.ToUpperInvariant()
        authenticodeSignerThumbprint = $thumbprint
    }
}

$manifestPath = Join-Path $OutputDirectory "berrykeep-server-node-stable.json"
$signaturePath = Join-Path $OutputDirectory "berrykeep-server-node-stable.json.p7s"
[System.IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json -Depth 4), [System.Text.UTF8Encoding]::new($false))

$content = [System.IO.File]::ReadAllBytes($manifestPath)
$cms = [System.Security.Cryptography.Pkcs.SignedCms]::new([System.Security.Cryptography.Pkcs.ContentInfo]::new($content), $true)
$signer = [System.Security.Cryptography.Pkcs.CmsSigner]::new($certificate)
$signer.IncludeOption = [Security.Cryptography.X509Certificates.X509IncludeOption]::EndCertOnly
$cms.ComputeSignature($signer)
[System.IO.File]::WriteAllBytes($signaturePath, $cms.Encode())

Write-Host "Created signed Server Node release manifest:"
Write-Host "  $manifestPath"
Write-Host "  $signaturePath"
