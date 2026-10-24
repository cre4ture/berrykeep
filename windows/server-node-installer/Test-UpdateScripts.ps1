[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$installerRoot = $PSScriptRoot
$scriptFiles = @(
    "Build-Msi.ps1",
    "ServerNodeUpdate.psm1",
    "Enable-ServerNodeAutoUpdate.ps1",
    "Disable-ServerNodeAutoUpdate.ps1",
    "Update-ServerNode.ps1",
    "Sign-Msi.ps1",
    "New-ReleaseManifest.ps1",
    (Join-Path $installerRoot "..\Verify-ExpectedAuthenticodeSignature.ps1")
)

foreach ($file in $scriptFiles) {
    $tokens = $null
    $errors = $null
    $path = if ([System.IO.Path]::IsPathRooted($file)) { $file } else { Join-Path $installerRoot $file }
    $null = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) {
        throw "PowerShell parser errors in ${file}: $($errors.Extent.Text -join '; ')"
    }
}

$updateModule = Import-Module (Join-Path $installerRoot "ServerNodeUpdate.psm1") -Force -PassThru
$temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("berrykeep-server-node-update-test-{0}" -f [Guid]::NewGuid())
New-Item -ItemType Directory -Path $temporaryDirectory -Force | Out-Null
try {
    $configurationPath = Join-Path $temporaryDirectory "server-node-update.json"
    $configuration = [pscustomobject]@{
        schemaVersion = 1
        enabled = $false
        manifestUri = "https://example.invalid/stable.json"
        signatureUri = "https://example.invalid/stable.json.p7s"
        signerThumbprint = ""
        maintenanceWindow = [pscustomobject]@{
            startLocal = "03:00"
            endLocal = "05:00"
        }
    }
    Save-ServerNodeUpdateConfiguration -Configuration $configuration -Path $configurationPath
    $loaded = Get-ServerNodeUpdateConfiguration -Path $configurationPath
    if ([bool]$loaded.enabled -or $loaded.maintenanceWindow.startLocal -ne "03:00") {
        throw "Server Node update configuration round-trip failed."
    }
    if (-not (Test-ServerNodeMaintenanceWindow -Configuration $loaded -Now ([DateTime]::Today.AddHours(3)))) {
        throw "Maintenance window should include its start boundary."
    }
    if (Test-ServerNodeMaintenanceWindow -Configuration $loaded -Now ([DateTime]::Today.AddHours(5))) {
        throw "Maintenance window should exclude its end boundary."
    }

    $certificate = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=BerryKeep Server Node updater test" -CertStoreLocation "Cert:\CurrentUser\My"
    try {
        # Exercise the real signing and manifest path with a small PE fixture.
        $installerPath = Join-Path $temporaryDirectory "signed-installer-fixture.dll"
        Add-Type -TypeDefinition 'public static class BerryKeepInstallerFixture { }' -OutputAssembly $installerPath -OutputType Library
        Set-AuthenticodeSignature -FilePath $installerPath -Certificate $certificate -HashAlgorithm SHA256 | Out-Null
        $certificatePath = Join-Path $temporaryDirectory "test-signing-certificate.pfx"
        $certificatePassword = [Guid]::NewGuid().ToString("N")
        $securePassword = ConvertTo-SecureString -String $certificatePassword -AsPlainText -Force
        Export-PfxCertificate -Cert $certificate -FilePath $certificatePath -Password $securePassword | Out-Null
        $installerUri = "https://example.invalid/berrykeep-server-node-1.2.3-windows-x64.msi"
        & (Join-Path $installerRoot "New-ReleaseManifest.ps1") `
            -MsiPath $installerPath `
            -Version "1.2.3" `
            -InstallerUri $installerUri `
            -SigningCertificatePath $certificatePath `
            -SigningCertificatePassword $certificatePassword `
            -OutputDirectory $temporaryDirectory

        $manifestPath = Join-Path $temporaryDirectory "berrykeep-server-node-stable.json"
        $signaturePath = "$manifestPath.p7s"
        $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
        if ($manifest.installer.url -ne $installerUri -or $manifest.version -ne "1.2.3") {
            throw "Generated release manifest must preserve its installer URL and version."
        }
        if ($manifest.installer.sha256 -ne (Get-FileHash -LiteralPath $installerPath -Algorithm SHA256).Hash) {
            throw "Generated release manifest must describe the signed installer payload."
        }
        & $updateModule {
            param($testManifestPath, $testSignaturePath, $thumbprint)
            Test-ServerNodeManifestSignature -ManifestPath $testManifestPath -SignaturePath $testSignaturePath -ExpectedThumbprint $thumbprint
        } $manifestPath $signaturePath $certificate.Thumbprint
    }
    finally {
        Remove-Item -LiteralPath $certificate.PSPath -Force -ErrorAction SilentlyContinue
    }
}
finally {
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host "Server Node updater script checks passed."
