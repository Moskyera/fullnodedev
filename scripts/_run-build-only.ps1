$ErrorActionPreference = "Continue"
$log = "C:\Users\KQHEX\Documents\hacash-fullnodedev\build-cycle.log"
$script = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\build-and-mine.ps1"

function Write-Log([string]$msg) {
    Add-Content -Path $log -Value "$(Get-Date -Format o) $msg" -Encoding UTF8
}

"=== BUILD START $(Get-Date -Format o) ===" | Out-File $log -Encoding UTF8

Write-Log "Invoking build-and-mine.ps1"
& $script 2>&1 | ForEach-Object { Write-Log $_ }
$code = $LASTEXITCODE
Write-Log "=== BUILD EXIT $code $(Get-Date -Format o) ==="
exit $code