$ErrorActionPreference = "Continue"
$log = "C:\Users\KQHEX\Documents\hacash-fullnodedev\full-cycle.log"
$script = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\build-and-mine.ps1"
$collect = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\_collect-after-mine.ps1"

function Write-Log([string]$msg) {
    $line = "$(Get-Date -Format o) $msg"
    Add-Content -Path $log -Value $line -Encoding UTF8
}

"=== FULL CYCLE START $(Get-Date -Format o) ===" | Out-File $log -Encoding UTF8

Write-Log "Running build-and-mine.ps1..."
$prevEap = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& $script *>> $log
$code = $LASTEXITCODE
$ErrorActionPreference = $prevEap
Write-Log "build-and-mine exit code: $code"

Write-Log "Waiting 90 seconds..."
Start-Sleep -Seconds 90

Write-Log "Collecting report..."
& $collect
Write-Log "=== FULL CYCLE DONE $(Get-Date -Format o) ==="