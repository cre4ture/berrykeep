<#
.SYNOPSIS
Disables and removes the automatic BerryKeep Server Node MSI updater task.
#>
[CmdletBinding()]
param([string]$ConfigurationPath = "")

$ErrorActionPreference = "Stop"
Import-Module (Join-Path $PSScriptRoot "ServerNodeUpdate.psm1") -Force

Assert-ServerNodeAdministrator

if ([string]::IsNullOrWhiteSpace($ConfigurationPath)) {
    $ConfigurationPath = Get-ServerNodeUpdateConfigurationPath
}

$configuration = Get-ServerNodeUpdateConfiguration -Path $ConfigurationPath
$configuration.enabled = $false
Save-ServerNodeUpdateConfiguration -Configuration $configuration -Path $ConfigurationPath
Unregister-ScheduledTask -TaskName "ServerNodeUpdate" -TaskPath "\BerryKeep\" -Confirm:$false -ErrorAction SilentlyContinue
Write-ServerNodeUpdateLog -Message "Disabled automatic Server Node updates."
Write-Host "Automatic BerryKeep Server Node updates are disabled."
