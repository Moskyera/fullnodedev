taskkill /F /IM poworker.exe 2>$null
taskkill /F /IM fullnode.exe 2>$null
taskkill /F /IM hacash.exe 2>$null
Start-Sleep 5

$log = "$env:TEMP\build-and-mine-final.log"
Remove-Item $log -ErrorAction SilentlyContinue
& powershell -NoProfile -ExecutionPolicy Bypass -File "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\build-and-mine.ps1" *>&1 | Tee-Object -FilePath $log
$code = $LASTEXITCODE
if ($null -eq $code) { $code = 0 }

"" | Out-File $env:TEMP\build-check-result.txt
"=== BUILD SCRIPT EXIT CODE: $code ===" | Out-File $env:TEMP\build-check-result.txt -Append
"=== LAST 30 LINES OF OUTPUT ===" | Out-File $env:TEMP\build-check-result.txt -Append
Get-Content $log -Tail 30 | Out-File $env:TEMP\build-check-result.txt -Append

Start-Sleep 60

"=== AFTER 60s: PROCESSES ===" | Out-File $env:TEMP\build-check-result.txt -Append
Get-Process poworker,fullnode -ErrorAction SilentlyContinue | Format-Table Id,ProcessName,CPU,WorkingSet -AutoSize | Out-String | Out-File $env:TEMP\build-check-result.txt -Append

"=== AFTER 60s: MINER STATS ===" | Out-File $env:TEMP\build-check-result.txt -Append
$statsPath = "C:\Users\KQHEX\Documents\hacash-fullnodedev\target\release\miner-stats.json"
if (Test-Path $statsPath) {
    Get-Content $statsPath -Raw | ConvertFrom-Json | Select-Object hashrate_display,gpu_hashrate_display,effective_work_groups | Format-List | Out-String | Out-File $env:TEMP\build-check-result.txt -Append
} else {
    "miner-stats.json not found" | Out-File $env:TEMP\build-check-result.txt -Append
}

exit $code