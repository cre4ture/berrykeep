param(
    [int]$FileCount = 2000,
    [int]$MinSizeMiB = 1,
    [int]$MaxSizeMiB = 5,
    [int]$VerifySampleCount = 24,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

if ($MinSizeMiB -le 0) {
    throw "MinSizeMiB must be greater than zero."
}
if ($MaxSizeMiB -lt $MinSizeMiB) {
    throw "MaxSizeMiB must be greater than or equal to MinSizeMiB."
}
if ($FileCount -le 0) {
    throw "FileCount must be greater than zero."
}

$repoRoot = Split-Path -Parent $PSScriptRoot

$env:IRONMESH_WINDOWS_CFAPI_LOAD_FILE_COUNT = [string]$FileCount
$env:IRONMESH_WINDOWS_CFAPI_LOAD_MIN_BYTES = [string]($MinSizeMiB * 1MB)
$env:IRONMESH_WINDOWS_CFAPI_LOAD_MAX_BYTES = [string]($MaxSizeMiB * 1MB)
$env:IRONMESH_WINDOWS_CFAPI_LOAD_VERIFY_SAMPLE_COUNT = [string]$VerifySampleCount

Write-Host "Running Windows CFAPI cluster workload"
Write-Host "  files          : $FileCount"
Write-Host "  size range MiB : $MinSizeMiB - $MaxSizeMiB"
Write-Host "  average MiB    : $([math]::Round(($MinSizeMiB + $MaxSizeMiB) / 2, 2))"
Write-Host "  sample checks  : $VerifySampleCount"

Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        cargo test --manifest-path tests\system-tests\Cargo.toml --no-run
    }

    cargo test `
        --manifest-path tests\system-tests\Cargo.toml `
        windows_cfapi_cluster_upload_and_replication_workload `
        -- `
        --ignored `
        --nocapture
}
finally {
    Pop-Location
}
