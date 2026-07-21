$ErrorActionPreference = "Continue"
$Report = "C:\Users\KQHEX\Documents\hacash-fullnodedev\mining-test-report.txt"
$Release = "C:\Users\KQHEX\Documents\hacash-fullnodedev\target\release"
$StartScript = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\start-hac-mining.ps1"

"=== MINING TEST $(Get-Date -Format o) ===" | Out-File $Report -Encoding UTF8

Add-Content $Report "=== Running start-hac-mining.ps1 -Restart ==="
& $StartScript -Restart 2>&1 | ForEach-Object { Add-Content $Report $_ }

Add-Content $Report ""
Add-Content $Report "=== Waiting 180 seconds ==="
Start-Sleep -Seconds 180

Add-Content $Report ""
Add-Content $Report "=== Get-Process poworker,fullnode ==="
$procs = Get-Process -Name poworker,fullnode -ErrorAction SilentlyContinue
if ($procs) {
    $procs | Format-Table -AutoSize | Out-String | Add-Content $Report
} else {
    Add-Content $Report "poworker/fullnode: NOT RUNNING"
}

Add-Content $Report ""
Add-Content $Report "=== miner-stats.json ==="
$statsPath = Join-Path $Release "miner-stats.json"
if (Test-Path $statsPath) {
    $stats = Get-Content $statsPath -Raw | ConvertFrom-Json
    Add-Content $Report "hashrate_display: $($stats.hashrate_display)"
    Add-Content $Report "gpu_hashrate_display: $($stats.gpu_hashrate_display)"
    Add-Content $Report "effective_work_groups: $($stats.effective_work_groups)"
    Add-Content $Report "gpu_hashrate_hps: $($stats.gpu_hashrate_hps)"
} else {
    Add-Content $Report "FILE NOT FOUND: $statsPath"
}

Add-Content $Report ""
Add-Content $Report "=== mining-live.err.log (last 10) ==="
$errLog = Join-Path $Release "mining-live.err.log"
if (Test-Path $errLog) {
    Get-Content $errLog -Tail 10 | Add-Content $Report
} else {
    Add-Content $Report "FILE NOT FOUND: $errLog"
}

Add-Content $Report ""
Add-Content $Report "=== mining-live.out.log (last 5) ==="
$outLog = Join-Path $Release "mining-live.out.log"
if (Test-Path $outLog) {
    Get-Content $outLog -Tail 5 | Add-Content $Report
} else {
    Add-Content $Report "FILE NOT FOUND: $outLog"
}

Add-Content $Report ""
Add-Content $Report "=== DONE $(Get-Date -Format o) ==="