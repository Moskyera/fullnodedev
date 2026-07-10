$ErrorActionPreference = 'Stop'
$bin = Join-Path $PSScriptRoot '..\..\target\release'
$cfg = Join-Path $bin 'poworker.config.ini'
# Keep a pristine backup before any edits (benchmark may patch ini mid-run).
$origBackup = "$cfg.origbak"
if (-not (Test-Path $origBackup)) {
    Copy-Item $cfg $origBackup -Force
}
$bak = "$cfg.diagbak"
$log = Join-Path $bin 'diagnose-benchmark.log'

Copy-Item $cfg $bak -Force
$t = Get-Content $cfg -Raw
$t = $t -replace '(?m)^cpu_assist\s*=\s*\w+', 'cpu_assist = false'
$t = $t -replace '(?m)^benchmark_seconds\s*=\s*\d+', 'benchmark_seconds = 45'
Set-Content $cfg $t -NoNewline

Push-Location $bin
try {
    cmd /c "poworker.exe > diagnose-benchmark.log 2>&1"
} finally {
    if (Test-Path $origBackup) {
        Copy-Item $origBackup $cfg -Force
    } elseif (Test-Path $bak) {
        Copy-Item $bak $cfg -Force
    }
    Remove-Item $bak -Force -ErrorAction SilentlyContinue
    Pop-Location
}