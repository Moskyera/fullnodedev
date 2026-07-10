# HACD + auto-bid smoke test (does not leave miners running).
param(
    [string]$Release = (Resolve-Path (Join-Path $PSScriptRoot "..\target\release")).Path,
    [string]$BidPassword = "hacd_smoke_test",
    [string]$RewardPrivakey = "1AhGNNrHUNaiwS2GWBPR4UuDXjEiDwoE3v",
    [int]$DiaworkerSeconds = 35
)

$ErrorActionPreference = "Stop"
$LockFile = Join-Path $env:TEMP "hacash-hacd-test.lock"
if (Test-Path $LockFile) {
    $age = (Get-Date) - (Get-Item $LockFile).LastWriteTime
    if ($age.TotalMinutes -lt 10) {
        throw "Another HACD test appears to be running (lock: $LockFile). Wait or delete the lock file."
    }
    Remove-Item $LockFile -Force -ErrorAction SilentlyContinue
}
New-Item -ItemType File -Path $LockFile -Force | Out-Null
trap {
    Stop-Workers
    if (Test-Path $HacBackup) {
        Copy-Item $HacBackup $HacIni -Force
        Remove-Item $HacBackup -Force -ErrorAction SilentlyContinue
        Write-Host "Restored hacash.config.ini from backup after error."
    }
    Remove-Item $LockFile -Force -ErrorAction SilentlyContinue
    throw $_
}
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$Report = Join-Path $Root "hacd-test-report.txt"
$HacIni = Join-Path $Release "hacash.config.ini"
$HacBackup = Join-Path $Release "hacash.config.ini.hacd-test.bak"
$DiaIni = Join-Path $Release "diaworker.config.ini"
$DiaOut = Join-Path $Release "hacd-test-dia.out.log"
$DiaErr = Join-Path $Release "hacd-test-dia.err.log"
$FnOut = Join-Path $Release "hacd-test-fn.out.log"
$FnErr = Join-Path $Release "hacd-test-fn.err.log"

function Log([string]$msg) {
    $line = "[$(Get-Date -Format 'HH:mm:ss')] $msg"
    Write-Host $line
    Add-Content -Path $Report -Value $line
}

function Stop-Workers {
    foreach ($n in @("poworker", "diaworker", "fullnode", "hacash")) {
        Get-Process -Name $n -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        cmd /c "taskkill /F /IM $n.exe 2>nul" | Out-Null
    }
    Start-Sleep -Seconds 2
}

function Wait-Rpc([int]$MaxSeconds = 90) {
    $deadline = (Get-Date).AddSeconds($MaxSeconds)
    while ((Get-Date) -lt $deadline) {
        try {
            $tcp = New-Object System.Net.Sockets.TcpClient
            $iar = $tcp.BeginConnect("127.0.0.1", 8080, $null, $null)
            if ($iar.AsyncWaitHandle.WaitOne(800)) {
                $tcp.EndConnect($iar) | Out-Null
                $tcp.Close()
                return $true
            }
            $tcp.Close()
        } catch {}
        Start-Sleep -Seconds 2
    }
    return $false
}

function Get-Api([string]$Path) {
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:8080$Path" -UseBasicParsing -TimeoutSec 8
        return @{ ok = $true; body = $r.Content }
    } catch {
        return @{ ok = $false; body = $_.Exception.Message }
    }
}

"=== HACD TEST $(Get-Date -Format o) ===" | Out-File $Report -Encoding UTF8
Log "Release dir: $Release"

Set-Location $Root
Log "Building diaworker + fullnode (ocl)..."
$prev = $ErrorActionPreference
$ErrorActionPreference = "Continue"
& cargo build --release --bin diaworker --bin fullnode --features ocl
$buildExit = $LASTEXITCODE
$ErrorActionPreference = $prev
if ($buildExit -ne 0) { throw "cargo build failed exit $buildExit" }
Log "Build OK"

Log "Running field diamond unit tests..."
& cargo test -p field test_diamond --quiet
if ($LASTEXITCODE -ne 0) { throw "field diamond tests failed" }
Log "field diamond tests OK"

Stop-Workers
Remove-Item $FnOut,$FnErr,$DiaOut,$DiaErr -ErrorAction SilentlyContinue
Start-Sleep -Seconds 3
if (Test-Path $HacIni) { Copy-Item $HacIni $HacBackup -Force }

# HACD mode: diamondminer on, block miner off
$diaOpencl = (Join-Path $Root "x16rs\opencl") -replace "\\", "/"
@"
; HACD smoke test config (restored after script)
connect = 127.0.0.1:8080
supervene = 6

[efficiency]
mode = profit
power_cost_kwh = 0.15
gpu_watts = 280
cpu_watts_per_thread = 8
hac_price = 0
dynamic_supervene = true
supervene_min = 2
supervene_max = 6
oom_fallback = true
max_temp_c = 83
throttle_work_groups = 64
idle_start_hour = 255
idle_end_hour = 255
pause_if_unprofitable = false
benchmark_seconds = 0
benchmark_fine_sweep = false
thermal_gpu_index = 0
stats_file = $((Join-Path $Release "miner-stats.json") -replace "\\", "/")

[gpu]
use_opencl = true
cpu_assist = true
gpu_slug = rx9070xt
gpu_profile = amd_balanced
platform_id = 0
device_ids = 0
opencl_dir = $diaOpencl/
work_groups = 64
local_size = 256
unit_size = 64
debug = 0
"@ | Set-Content -Path $DiaIni -Encoding UTF8
Log "Wrote diaworker.config.ini (WG=64 rx9070xt)"

# Patch hacash.config.ini: [miner] off, [diamondminer] on with test credentials
$hac = Get-Content $HacIni -Raw
if ($hac -match '(?ms)\[miner\]') {
    $hac = $hac -replace '(?ms)(\[miner\][^\[]*?)^enable\s*=.*', '${1}enable = false'
} else {
    $hac += "`n[miner]`nenable = false`nreward = $RewardPrivakey`n"
}
$dmerBlock = @"
[diamondminer]
enable = true
reward = $RewardPrivakey
bid_password = $BidPassword
bid_min = 1:0
bid_max = 31:0
bid_step = 1:0
"@
if ($hac -match '(?ms)\[diamondminer\]') {
    $hac = $hac -replace '(?ms)\[diamondminer\][^\[]*', "$dmerBlock`n"
} else {
    $hac += "`n$dmerBlock`n"
}
Set-Content -Path $HacIni -Value $hac -NoNewline
Log "Patched hacash.config.ini for HACD (test PRIVAKEY + bid_password)"

Log "Starting fullnode..."
$fn = Join-Path $Release "fullnode.exe"
$fnProc = Start-Process -FilePath $fn -WorkingDirectory $Release -RedirectStandardOutput $FnOut -RedirectStandardError $FnErr -PassThru -WindowStyle Hidden
if (-not (Wait-Rpc 120)) {
    if ($fnProc -and -not $fnProc.HasExited) { Stop-Process -Id $fnProc.Id -Force -ErrorAction SilentlyContinue }
    if (Test-Path $FnErr) { Get-Content $FnErr -Tail 8 | ForEach-Object { Log "  fnerr: $_" } }
    if (Test-Path $FnOut) { Get-Content $FnOut -Tail 8 | ForEach-Object { Log "  fnout: $_" } }
    throw "fullnode did not open 8080"
}
Log "fullnode RPC OK"
Start-Sleep -Seconds 35

$latest = Get-Api "/query/latest"
Log "GET /query/latest -> $($latest.body.Substring(0, [Math]::Min(120, $latest.body.Length)))"

$bidding = Get-Api "/query/diamond/bidding"
Log "GET /query/diamond/bidding -> $($bidding.body.Substring(0, [Math]::Min(160, $bidding.body.Length)))"

$init = Get-Api "/query/diamondminer/init"
Log "GET /query/diamondminer/init -> $($init.body)"
if ($init.body -notmatch '"bid_address"') { throw "diamondminer/init missing bid_address" }
Log "diamondminer/init OK"

Log "Starting diaworker for ${DiaworkerSeconds}s..."
$dia = Join-Path $Release "diaworker.exe"
Remove-Item $DiaOut,$DiaErr -ErrorAction SilentlyContinue
$p = Start-Process -FilePath $dia -WorkingDirectory $Release -RedirectStandardOutput $DiaOut -RedirectStandardError $DiaErr -PassThru -WindowStyle Hidden
Start-Sleep -Seconds $DiaworkerSeconds
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue }

$diaText = ""
if (Test-Path $DiaOut) { $diaText += (Get-Content $DiaOut -Raw) }
if (Test-Path $DiaErr) { $diaText += (Get-Content $DiaErr -Raw) }
$diaTail = @()
if (Test-Path $DiaOut) { $diaTail += Get-Content $DiaOut -Tail 8 }
Log "diaworker log tail:"
$diaTail | ForEach-Object { Log "  $_" }

$okGfx = $diaText -match "gfx1201|Device 0"
$okInit = $diaText -match "bid address|query diamond miner"
$okMining = $diaText -match "Create GPU diamond miner worker|GH/s"
if (-not $okInit) { throw "diaworker did not reach diamondminer init" }
Log "diaworker init path OK"
if ($okGfx) { Log "OpenCL gfx1201 detected" }
if ($okMining) { Log "diaworker reached active hashing" }
if ($diaText -match "MINING SUCCESS") { Log "diaworker found at least one diamond share (submit may still fail on local chain)" }

Stop-Workers
if (Test-Path $HacBackup) {
    Copy-Item $HacBackup $HacIni -Force
    Remove-Item $HacBackup -Force
    Log "Restored hacash.config.ini from backup"
}

Log "=== HACD SMOKE TEST PASSED ==="
Remove-Item $LockFile -Force -ErrorAction SilentlyContinue
Write-Host ""
Write-Host "Report: $Report" -ForegroundColor Green