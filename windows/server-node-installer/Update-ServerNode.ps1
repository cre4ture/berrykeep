<#
.SYNOPSIS
Checks the signed stable release manifest and applies one BerryKeep Server Node MSI update.
#>
[CmdletBinding()]
param(
    [string]$ConfigurationPath = "",
    [switch]$Scheduled,
    [switch]$Force,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
Import-Module (Join-Path $PSScriptRoot "ServerNodeUpdate.psm1") -Force

if ([string]::IsNullOrWhiteSpace($ConfigurationPath)) {
    $ConfigurationPath = Get-ServerNodeUpdateConfigurationPath
}

try {
    $result = Invoke-ServerNodeUpdate -ConfigurationPath $ConfigurationPath -Force:$Force -DryRun:$DryRun
    if (-not $Scheduled) {
        $result | Format-List | Out-Host
    }
    exit 0
}
catch {
    Write-ServerNodeUpdateLog -Level "ERROR" -Message "Update command failed: $($_.Exception.Message)"
    if (-not $Scheduled) {
        throw
    }
    exit 1
}
