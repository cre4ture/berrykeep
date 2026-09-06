param(
    [string]$Bind = "127.0.0.1:18443",
    [string]$DataDir = "",
    [string]$RustLog = "info",
    [switch]$Build
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
$binaryPath = Join-Path $repoRoot "target\debug\berrykeep-server-node.exe"

if ([string]::IsNullOrWhiteSpace($DataDir)) {
    $DataDir = Join-Path $repoRoot "target\tmp\server-node-dev"
}

if ($Build -or -not (Test-Path $binaryPath)) {
    Push-Location $repoRoot
    try {
        cargo build --locked -p server-node --bin berrykeep-server-node
    }
    finally {
        Pop-Location
    }
}

New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

$env:IRONMESH_SERVER_BIND = $Bind
$env:IRONMESH_DATA_DIR = $DataDir
$env:RUST_LOG = $RustLog

Write-Host "Starting berrykeep-server-node.exe"
Write-Host "  bind: $Bind"
Write-Host "  data: $DataDir"
Write-Host "  log : $RustLog"

Push-Location $repoRoot
try {
    & $binaryPath
}
finally {
    Pop-Location
}
