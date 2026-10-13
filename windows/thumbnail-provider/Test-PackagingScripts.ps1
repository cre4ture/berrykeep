[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$scriptFiles = @(
    'Build-PrototypePackage.ps1',
    'Build-StoreUploadPackage.ps1',
    'New-MsixUploadPackage.ps1',
    'Sign-Msix.ps1',
    (Join-Path $PSScriptRoot '..\Verify-ExpectedAuthenticodeSignature.ps1')
)

foreach ($file in $scriptFiles) {
    $tokens = $null
    $errors = $null
    $path = if ([System.IO.Path]::IsPathRooted($file)) { $file } else { Join-Path $PSScriptRoot $file }
    $null = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$tokens, [ref]$errors)
    if ($errors.Count -gt 0) {
        throw "PowerShell parser errors in ${file}: $($errors.Extent.Text -join '; ')"
    }
}

[xml]$manifest = Get-Content -Raw -Path (Join-Path $PSScriptRoot 'AppxManifest.xml')
$namespaces = [System.Xml.XmlNamespaceManager]::new($manifest.NameTable)
$namespaces.AddNamespace('f', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10')
$namespaces.AddNamespace('desktop6', 'http://schemas.microsoft.com/appx/manifest/desktop/windows10/6')
$namespaces.AddNamespace('virtualization', 'http://schemas.microsoft.com/appx/manifest/virtualization/windows10')
$namespaces.AddNamespace('rescap', 'http://schemas.microsoft.com/appx/manifest/foundation/windows10/restrictedcapabilities')

$legacyVirtualization = $manifest.SelectSingleNode('/f:Package/f:Properties/desktop6:FileSystemWriteVirtualization', $namespaces)
if ($null -eq $legacyVirtualization -or $legacyVirtualization.InnerText.Trim() -ne 'disabled') {
    throw 'AppxManifest.xml must disable legacy AppData file-system write virtualization for shared BerryKeep state.'
}

$externalStateDirectories = @($manifest.SelectNodes('/f:Package/f:Properties/virtualization:FileSystemWriteVirtualization/virtualization:ExcludedDirectories/virtualization:ExcludedDirectory', $namespaces) | Where-Object { $_.InnerText.Trim() -eq '$(KnownFolder:LocalAppData)\BerryKeep' })
if ($externalStateDirectories.Count -eq 0) {
    throw 'AppxManifest.xml must exclude %LOCALAPPDATA%\BerryKeep from Windows 11+ file-system write virtualization.'
}

$unvirtualizedResources = $manifest.SelectSingleNode('/f:Package/f:Capabilities/rescap:Capability[@Name="unvirtualizedResources"]', $namespaces)
if ($null -eq $unvirtualizedResources) {
    throw 'AppxManifest.xml must declare the unvirtualizedResources restricted capability.'
}

$temporaryDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ("berrykeep-msix-package-test-{0}" -f [Guid]::NewGuid())
New-Item -ItemType Directory -Path $temporaryDirectory -Force | Out-Null
try {
    $msixPath = Join-Path $temporaryDirectory 'fixture.msix'
    $appxSymPath = Join-Path $temporaryDirectory 'fixture.appxsym'
    $uploadPath = Join-Path $temporaryDirectory 'fixture.msixupload'
    [System.IO.File]::WriteAllBytes($msixPath, [byte[]](1, 2, 3))
    [System.IO.File]::WriteAllBytes($appxSymPath, [byte[]](4, 5, 6))

    & (Join-Path $PSScriptRoot 'New-MsixUploadPackage.ps1') `
        -MsixPath $msixPath `
        -AppxSymPath $appxSymPath `
        -OutputPath $uploadPath

    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($uploadPath)
    try {
        $entryNames = @($archive.Entries | ForEach-Object FullName | Sort-Object)
        if ($entryNames.Count -ne 2 -or $entryNames[0] -ne 'fixture.appxsym' -or $entryNames[1] -ne 'fixture.msix') {
            throw "MSIX upload archive has unexpected entries: $($entryNames -join ', ')"
        }
    }
    finally {
        $archive.Dispose()
    }
}
finally {
    Remove-Item -LiteralPath $temporaryDirectory -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host 'Windows client packaging script checks passed.'
