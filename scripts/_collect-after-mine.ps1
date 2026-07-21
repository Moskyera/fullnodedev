$ErrorActionPreference = "Continue"
$Release = "C:\Users\KQHEX\Documents\hacash-fullnodedev\target\release"
$Report = "C:\Users\KQHEX\Documents\hacash-fullnodedev\mine-report.txt"

function Add-Section([string]$title) {
    Add-Content -Path $Report -Value ""
    Add-Content -Path $Report -Value "=== $title ==="
}

"=== MINE REPORT $(Get-Date -Format o) ===" | Out-File $Report -Encoding UTF8

Add-Section "Get-Process poworker,fullnode"
Get-Process -Name poworker,fullnode -ErrorAction SilentlyContinue | Format-Table -AutoSize | Out-String | Add-Content $Report
if (-not (Get-Process -Name poworker -ErrorAction SilentlyContinue)) {
    "poworker: NOT RUNNING" | Add-Content $Report
}
if (-not (Get-Process -Name fullnode -ErrorAction SilentlyContinue)) {
    "fullnode: NOT RUNNING" | Add-Content $Report
}

Add-Section "mining-live.out.log (last 25)"
$outLog = Join-Path $Release "mining-live.out.log"
if (Test-Path $outLog) {
    Get-Content $outLog -Tail 25 | Add-Content $Report
} else {
    "FILE NOT FOUND: $outLog" | Add-Content $Report
}

Add-Section "mining-live.err.log (last 25)"
$errLog = Join-Path $Release "mining-live.err.log"
if (Test-Path $errLog) {
    Get-Content $errLog -Tail 25 | Add-Content $Report
} else {
    "FILE NOT FOUND: $errLog" | Add-Content $Report
}

Add-Section "miner-stats.json (raw)"
$stats = Join-Path $Release "miner-stats.json"
if (Test-Path $stats) {
    Get-Content $stats -Raw | Add-Content $Report
} else {
    "FILE NOT FOUND: $stats" | Add-Content $Report
}

Add-Section "DONE"