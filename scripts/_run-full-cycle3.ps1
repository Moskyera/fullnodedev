$ErrorActionPreference = "Continue"
$log = "C:\Users\KQHEX\Documents\hacash-fullnodedev\full-cycle3.log"
$script = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\build-and-mine.ps1"
$collect = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\_collect-after-mine.ps1"

function Write-Log([string]$msg) {
    Add-Content -Path $log -Value "$(Get-Date -Format o) $msg" -Encoding UTF8
}

"=== CYCLE3 START $(Get-Date -Format o) ===" | Out-File $log -Encoding UTF8

Write-Log "Running build-and-mine.ps1"
& $script 2>&1 | ForEach-Object { Write-Log $_ }
$code = $LASTEXITCODE
Write-Log "build-and-mine exit code: $code"

Write-Log "Waiting 90 seconds"
Start-Sleep -Seconds 90

Write-Log "Collecting report"
& $collect
Write-Log "=== CYCLE3 DONE $(Get-Date -Format o) ==="