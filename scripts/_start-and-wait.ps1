$ErrorActionPreference = "Continue"
$log = "C:\Users\KQHEX\Documents\hacash-fullnodedev\start-wait.log"
$r = "C:\Users\KQHEX\Documents\hacash-fullnodedev\target\release"
$collect = "C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\_collect-after-mine.ps1"

function Write-Log([string]$msg) {
    Add-Content -Path $log -Value "$(Get-Date -Format o) $msg" -Encoding UTF8
}

"=== START-WAIT $(Get-Date -Format o) ===" | Out-File $log -Encoding UTF8

Write-Log "Stopping miners"
Get-Process -Name poworker,fullnode,hacash -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 3

Write-Log "cargo build"
Set-Location (Join-Path $r "..\\..")
cargo build --release --bin poworker --bin fullnode --features ocl 2>&1 | ForEach-Object { Write-Log $_ }
if ($LASTEXITCODE -ne 0) {
    Write-Log "cargo failed $LASTEXITCODE"
    exit $LASTEXITCODE
}

Write-Log "Starting fullnode"
Start-Process -FilePath (Join-Path $r "fullnode.exe") -WorkingDirectory $r -WindowStyle Hidden

Write-Log "Waiting for miner API"
$ready = $false
$deadline = (Get-Date).AddSeconds(120)
while ((Get-Date) -lt $deadline) {
    if (-not (Get-Process -Name fullnode -ErrorAction SilentlyContinue)) {
        Write-Log "fullnode exited early"
        exit 1
    }
    try {
        $c = (Invoke-WebRequest -Uri "http://127.0.0.1:8080/query/miner/pending?stuff=true" -UseBasicParsing -TimeoutSec 3).Content
        if ($c -notmatch "30 secs after node start") {
            $ready = $true
            Write-Log "miner API ready"
            break
        }
    } catch {}
    Start-Sleep -Seconds 3
}
if (-not $ready) {
    Write-Log "miner API timeout"
    exit 1
}

Get-ChildItem -Path $r -Filter "*gfx1201*.bin" -ErrorAction SilentlyContinue | Remove-Item -Force
Write-Log "Starting poworker"
$out = Join-Path $r "mining-live.out.log"
$err = Join-Path $r "mining-live.err.log"
Start-Process -FilePath (Join-Path $r "poworker.exe") -WorkingDirectory $r -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden

Write-Log "Waiting 90 seconds"
Start-Sleep -Seconds 90

Write-Log "Collecting"
& $collect
Write-Log "=== START-WAIT DONE $(Get-Date -Format o) ==="