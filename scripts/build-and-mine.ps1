# Stop miners, rebuild, then start with -Restart (avoids exe lock during cargo build).
$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Set-Location $Root

$lock = Join-Path $Root "build-and-mine.lock"
if (Test-Path $lock) {
    $age = (Get-Date) - (Get-Item $lock).LastWriteTime
    if ($age.TotalMinutes -lt 10) {
        Write-Error "build-and-mine already running (lock file younger than 10 min). Wait or delete $lock"
    }
    Remove-Item $lock -Force
}
New-Item -Path $lock -ItemType File -Force | Out-Null
try {
    function Stop-Miners {
        foreach ($name in @("poworker", "fullnode", "hacash")) {
            Get-Process -Name $name -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
            cmd /c "taskkill /F /IM $name.exe 2>nul" | Out-Null
        }
    }

    Stop-Miners
    Start-Sleep -Seconds 3
    Stop-Miners
    Start-Sleep -Seconds 2

    $locked = Get-Process -Name poworker,fullnode,hacash -ErrorAction SilentlyContinue
    if ($locked) {
        Write-Error "Cannot build: still running: $($locked.ProcessName -join ', '). Close miner-panel or kill manually."
    }

    Write-Host "Building poworker + fullnode..."
    # Cargo prints warnings to stderr; do not let $ErrorActionPreference=Stop treat them as fatal.
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    & cargo build --release --bin poworker --bin fullnode --features ocl
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = $prevEap
    if ($buildExit -ne 0) { exit $buildExit }

    & (Join-Path $PSScriptRoot "start-hac-mining.ps1") -Restart -ForceOpenCLRebuild
} finally {
    Remove-Item $lock -Force -ErrorAction SilentlyContinue
}