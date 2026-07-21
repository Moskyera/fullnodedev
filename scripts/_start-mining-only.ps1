$log = "C:\Users\KQHEX\Documents\hacash-fullnodedev\start-mining.log"
$script = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\start-hac-mining.ps1"
"=== START $(Get-Date -Format o) ===" | Out-File $log -Encoding UTF8
& $script -Restart 2>&1 | ForEach-Object { Add-Content $log $_ }
"=== EXIT $LASTEXITCODE $(Get-Date -Format o) ===" | Add-Content $log