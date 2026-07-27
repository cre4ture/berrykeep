<#
.SYNOPSIS
Enables the opt-in automatic BerryKeep Server Node MSI updater.
#>
[CmdletBinding()]
param(
    [string]$ConfigurationPath = "",
    [string]$ManifestUri = "",
    [string]$SignatureUri = "",
    [string]$MaintenanceWindowStart = "",
    [string]$MaintenanceWindowEnd = ""
)

$ErrorActionPreference = "Stop"
Import-Module (Join-Path $PSScriptRoot "ServerNodeUpdate.psm1") -Force

function Get-TaskRunnerCommand {
    param([Parameter(Mandatory)][string]$UpdateScriptPath)

    $escapedPath = $UpdateScriptPath.Replace("'", "''")
    $script = @'
$updateScript = '__UPDATE_SCRIPT__'
if (Test-Path -LiteralPath $updateScript -PathType Leaf) {
    & $updateScript -Scheduled
}
else {
    Unregister-ScheduledTask -TaskName 'ServerNodeUpdate' -TaskPath '\BerryKeep\' -Confirm:$false -ErrorAction SilentlyContinue
}
'@.Replace("__UPDATE_SCRIPT__", $escapedPath)
    return [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($script))
}

Assert-ServerNodeAdministrator
if ([string]::IsNullOrWhiteSpace($ConfigurationPath)) {
    $ConfigurationPath = Get-ServerNodeUpdateConfigurationPath
}

$configuration = Get-ServerNodeUpdateConfiguration -Path $ConfigurationPath
if (-not [string]::IsNullOrWhiteSpace($ManifestUri)) {
    $configuration.manifestUri = (Assert-ServerNodeHttpsUri -Value $ManifestUri -Name "-ManifestUri").AbsoluteUri
}
if (-not [string]::IsNullOrWhiteSpace($SignatureUri)) {
    $configuration.signatureUri = (Assert-ServerNodeHttpsUri -Value $SignatureUri -Name "-SignatureUri").AbsoluteUri
}
if (-not [string]::IsNullOrWhiteSpace($MaintenanceWindowStart)) {
    $configuration.maintenanceWindow.startLocal = $MaintenanceWindowStart
}
if (-not [string]::IsNullOrWhiteSpace($MaintenanceWindowEnd)) {
    $configuration.maintenanceWindow.endLocal = $MaintenanceWindowEnd
}
if ([string]$configuration.signerThumbprint -notmatch '^[0-9A-Fa-f]{40}$') {
    throw "This MSI does not contain a release signing certificate thumbprint. Install a signed release MSI before enabling automatic updates."
}

$configuration.enabled = $true
$null = Test-ServerNodeMaintenanceWindow -Configuration $configuration
Save-ServerNodeUpdateConfiguration -Configuration $configuration -Path $ConfigurationPath

$windowStart = [DateTime]::ParseExact([string]$configuration.maintenanceWindow.startLocal, "HH:mm", [Globalization.CultureInfo]::InvariantCulture)
$updateScript = Join-Path $PSScriptRoot "Update-ServerNode.ps1"
$encodedCommand = Get-TaskRunnerCommand -UpdateScriptPath $updateScript
$action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument "-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand $encodedCommand"
$trigger = New-ScheduledTaskTrigger -Daily -At $windowStart
$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Minutes 45) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
$principal = New-ScheduledTaskPrincipal -UserId "SYSTEM" -LogonType ServiceAccount -RunLevel Highest
$task = New-ScheduledTask -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Description "Checks signed BerryKeep Server Node releases and installs them only during the configured maintenance window."

try {
    Register-ScheduledTask -TaskName "ServerNodeUpdate" -TaskPath "\BerryKeep\" -InputObject $task -Force | Out-Null
}
catch {
    $configuration.enabled = $false
    Save-ServerNodeUpdateConfiguration -Configuration $configuration -Path $ConfigurationPath
    throw
}

Write-ServerNodeUpdateLog -Message "Enabled automatic Server Node updates. Daily maintenance window: $($configuration.maintenanceWindow.startLocal)-$($configuration.maintenanceWindow.endLocal) local time."
Write-Host "Automatic BerryKeep Server Node updates are enabled."
Write-Host "Task: \\BerryKeep\\ServerNodeUpdate (runs as SYSTEM)"
