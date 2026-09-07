<#
.SYNOPSIS
Creates a Partner Center .msixupload archive from a signed or unsigned MSIX.

.DESCRIPTION
The archive contains the supplied MSIX and, when provided, its .appxsym
payload. It does not inspect or copy any certificate material.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$MsixPath,
    [string]$AppxSymPath,
    [Parameter(Mandatory)][string]$OutputPath
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not (Test-Path -LiteralPath $MsixPath -PathType Leaf)) {
    throw "MSIX was not found: $MsixPath"
}
if ($AppxSymPath -and -not (Test-Path -LiteralPath $AppxSymPath -PathType Leaf)) {
    throw "AppxSym file was not found: $AppxSymPath"
}

$outputDirectory = Split-Path -Parent $OutputPath
if ([string]::IsNullOrWhiteSpace($outputDirectory)) {
    throw '-OutputPath must include a directory.'
}
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null

$stagingDirectory = Join-Path $outputDirectory ('.msixupload-stage-' + [Guid]::NewGuid().ToString('N'))
$zipPath = [System.IO.Path]::ChangeExtension($OutputPath, '.zip')
New-Item -ItemType Directory -Path $stagingDirectory -Force | Out-Null
try {
    Copy-Item -LiteralPath $MsixPath -Destination (Join-Path $stagingDirectory (Split-Path -Leaf $MsixPath))
    if ($AppxSymPath) {
        Copy-Item -LiteralPath $AppxSymPath -Destination (Join-Path $stagingDirectory (Split-Path -Leaf $AppxSymPath))
    }

    Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $OutputPath -Force -ErrorAction SilentlyContinue
    Compress-Archive -LiteralPath (Get-ChildItem -LiteralPath $stagingDirectory -File | Select-Object -ExpandProperty FullName) -DestinationPath $zipPath -CompressionLevel Optimal
    Move-Item -LiteralPath $zipPath -Destination $OutputPath
}
finally {
    Remove-Item -LiteralPath $stagingDirectory -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
}

Write-Host 'Created MSIX upload package:' -ForegroundColor Green
Write-Host "  $OutputPath"
