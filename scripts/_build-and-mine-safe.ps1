# Same as build-and-mine.ps1 but tolerates cargo stderr warnings (Stop breaks on warning: lines).
$ErrorActionPreference = "Continue"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $Root

Get-Process -Name poworker,fullnode,hacash -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 3

Write-Host "Building poworker + fullnode..."
cargo build --release --bin poworker --bin fullnode --features ocl
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

& (Join-Path $PSScriptRoot "start-hac-mining.ps1") -Restart