$ErrorActionPreference = "Continue"
$resultPath = "C:\Users\KQHEX\Documents\hacash-fullnodedev\bm-check-out.txt"
$log = "$env:TEMP\build-and-mine-out.txt"
$err = "$env:TEMP\build-and-mine-err.txt"
$statsPath = "C:\Users\KQHEX\Documents\hacash-fullnodedev\target\release\miner-stats.json"
Remove-Item $log, $err -ErrorAction SilentlyContinue

$p = Start-Process -FilePath "powershell.exe" `
    -ArgumentList "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\build-and-mine.ps1" `
    -RedirectStandardOutput $log `
    -RedirectStandardError $err `
    -PassThru -NoNewWindow

Start-Sleep -Seconds 45

Write-Host "=== AFTER 45s: Get-Process poworker,fullnode ==="
$procs = Get-Process -Name poworker, fullnode -ErrorAction SilentlyContinue
if ($procs) {
    $procs | Format-Table Id, ProcessName, CPU, WorkingSet -AutoSize
} else {
    Write-Host "(no poworker/fullnode processes)"
}

Write-Host "=== AFTER 45s: miner-stats ==="
if (Test-Path $statsPath) {
    Get-Content $statsPath -Raw | ConvertFrom-Json |
        Select-Object hashrate_display, gpu_hashrate_display, effective_work_groups |
        Format-List
} else {
    Write-Host "miner-stats.json not found at $statsPath"
}

$p.WaitForExit()
$exitCode = $p.ExitCode
Write-Host "=== EXIT CODE: $exitCode ==="

$all = @()
if (Test-Path $log) { $all += Get-Content $log }
if (Test-Path $err) { $all += Get-Content $err }

Write-Host "=== LAST 20 LINES ==="
if ($all.Count -gt 0) {
    $all | Select-Object -Last 20
} else {
    Write-Host "(no output captured)"
}

$report = @()
$report += "=== AFTER 45s: Get-Process poworker,fullnode ==="
if ($procs) {
    $report += ($procs | Format-Table Id, ProcessName, CPU, WorkingSet -AutoSize | Out-String)
} else {
    $report += "(no poworker/fullnode processes)"
}
$report += "=== AFTER 45s: miner-stats ==="
if (Test-Path $statsPath) {
    $report += (Get-Content $statsPath -Raw | ConvertFrom-Json |
        Select-Object hashrate_display, gpu_hashrate_display, effective_work_groups |
        Format-List | Out-String)
} else {
    $report += "miner-stats.json not found at $statsPath"
}
$report += "=== EXIT CODE: $exitCode ==="
$report += "=== LAST 20 LINES ==="
if ($all.Count -gt 0) {
    $report += ($all | Select-Object -Last 20)
} else {
    $report += "(no output captured)"
}
$report | Out-File -FilePath $resultPath -Encoding utf8

exit $exitCode