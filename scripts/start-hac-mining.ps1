# Start fullnode (if needed) + poworker for solo mining on RX 9070 XT / gfx1201.
param(
    [switch]$Restart,
    [switch]$ForceOpenCLRebuild
)

$ErrorActionPreference = "Stop"
$Release = (Resolve-Path (Join-Path $PSScriptRoot "..\target\release")).Path
Set-Location $Release

function Test-Rpc {
    param([string]$HostPort = "127.0.0.1:8080")
    try {
        $parts = $HostPort.Split(":")
        $tcp = New-Object System.Net.Sockets.TcpClient
        $iar = $tcp.BeginConnect($parts[0], [int]$parts[1], $null, $null)
        $ok = $iar.AsyncWaitHandle.WaitOne(800)
        if ($ok) { $tcp.EndConnect($iar) | Out-Null }
        $tcp.Close()
        return $ok
    } catch { return $false }
}

function Test-MinerReady {
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:8080/query/miner/pending?stuff=true" -UseBasicParsing -TimeoutSec 5
        $c = $r.Content
        if ($c -match '"block_intro"') { return $true }
        if ($c -match "30 secs after node start") { return $false }
        if ($c -match '"ret"\s*:\s*0') { return $true }
        return $false
    } catch { return $false }
}

function Wait-MinerReady {
    param([int]$MaxSeconds = 120)
    $deadline = (Get-Date).AddSeconds($MaxSeconds)
    $missing = 0
    while ((Get-Date) -lt $deadline) {
        if (-not (Get-Process -Name fullnode,hacash -ErrorAction SilentlyContinue)) {
            $missing++
            if ($missing -ge 4) {
                Write-Error "fullnode exited before miner API became ready (see fullnode-live.err.log)."
            }
            Start-Sleep -Seconds 2
            continue
        }
        $missing = 0
        if (Test-MinerReady) {
            Write-Host "Miner API ready."
            return
        }
        Write-Host "  waiting for miner API (30s node warmup)..."
        Start-Sleep -Seconds 3
    }
    Write-Error "Miner API not ready within ${MaxSeconds}s (see fullnode-live.out.log / port 8080)."
}

function Stop-Miners {
    foreach ($name in @("poworker", "fullnode", "hacash")) {
        Get-Process -Name $name -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
        cmd /c "taskkill /F /IM $name.exe 2>nul" | Out-Null
    }
}

function Wait-PortFree {
    param([int]$MaxSeconds = 20)
    $deadline = (Get-Date).AddSeconds($MaxSeconds)
    while ((Test-Rpc) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 1
    }
}

if ($Restart) {
    Stop-Miners
    Start-Sleep -Seconds 3
    if (Test-Rpc) {
        Write-Host "  waiting for port 8080 to close..."
        Wait-PortFree
    }
}

# Ensure a single poworker instance (leave fullnode running unless -Restart).
Get-Process -Name poworker -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
cmd /c "taskkill /F /IM poworker.exe 2>nul" | Out-Null
Start-Sleep -Seconds 2

# gfx1201 stable path: WG=64 US=64 (avoids RDNA4 OOM cascade)
$ini = Join-Path $Release "poworker.config.ini"
if (-not (Test-Path $ini)) {
    Write-Error "Missing $ini - open miner-panel, Save, then re-run this script."
}
$content = Get-Content $ini -Raw
if ($content -notmatch "gpu_slug\s*=\s*rx9070xt") {
    $content = $content -replace "(?m)^gpu_slug\s*=.*", "gpu_slug = rx9070xt"
}
$content = $content -replace "(?m)^work_groups\s*=.*", "work_groups = 64"
$content = $content -replace "(?m)^unit_size\s*=.*", "unit_size = 64"
$content = $content -replace "(?m)^throttle_work_groups\s*=.*", "throttle_work_groups = 64"
$stats = (Join-Path $Release "miner-stats.json") -replace "\\", "/"
if ($content -notmatch "stats_file\s*=") {
    $content = $content -replace "(\[efficiency\])", "`$1`nstats_file = $stats"
} else {
    $content = $content -replace "(?m)^stats_file\s*=.*", "stats_file = $stats"
}
Set-Content -Path $ini -Value $content -NoNewline

$fullnode = Join-Path $Release "fullnode.exe"
$poworker = Join-Path $Release "poworker.exe"
if (-not (Test-Path $fullnode)) { Write-Error "Build first: cargo build --release --bin fullnode --bin poworker --features ocl" }
if (-not (Test-Path $poworker)) { Write-Error "Build first: cargo build --release --bin poworker --features ocl" }

$needFullnode = $Restart -or -not (Test-Rpc) -or -not (Get-Process -Name fullnode,hacash -ErrorAction SilentlyContinue)
if ($needFullnode) {
    if (-not $Restart) { Stop-Miners; Start-Sleep -Seconds 2; Wait-PortFree }
    Write-Host "Starting fullnode..."
    $fnOut = Join-Path $Release "fullnode-live.out.log"
    $fnErr = Join-Path $Release "fullnode-live.err.log"
    Start-Process -FilePath $fullnode -WorkingDirectory $Release `
        -RedirectStandardOutput $fnOut -RedirectStandardError $fnErr -WindowStyle Hidden
    $deadline = (Get-Date).AddSeconds(120)
    while (-not (Test-Rpc) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
        Write-Host "  waiting for RPC 127.0.0.1:8080..."
    }
    if (-not (Test-Rpc)) { Write-Error "Fullnode did not open port 8080 within 120s." }
}
if ($Restart -or $needFullnode) {
    Wait-MinerReady -MaxSeconds 120
} else {
    $fn = Get-Process -Name fullnode,hacash -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($fn -and ((Get-Date) - $fn.StartTime).TotalSeconds -lt 45) {
        Wait-MinerReady -MaxSeconds 120
    } elseif (-not (Test-MinerReady)) {
        Write-Host "Miner API not ready on existing fullnode; waiting..."
        Wait-MinerReady -MaxSeconds 120
    }
}

Get-Process -Name poworker -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

# Drop cached gfx1201 binaries only when kernels changed or -ForceOpenCLRebuild.
$oclDir = Join-Path (Resolve-Path (Join-Path $PSScriptRoot "..\x16rs\opencl")).Path
$kernelFiles = @(
    "x16rs.cl", "x16rs_main.cl", "x16rs_diamond.cl", "groestl.cl", "aes_helper.cl"
) | ForEach-Object { Join-Path $oclDir $_ }
$kernelNewest = ($kernelFiles | Where-Object { Test-Path $_ } | Get-Item |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1).LastWriteTime
$bins = Get-ChildItem -Path $oclDir -Filter "*gfx1201*.bin" -ErrorAction SilentlyContinue
$stale = $ForceOpenCLRebuild -or -not $bins -or ($bins | Where-Object { $_.LastWriteTime -lt $kernelNewest })
if ($stale -and $bins) {
    Write-Host "Removing stale gfx1201 OpenCL cache (kernel sources newer than binary)..."
    $bins | Remove-Item -Force
} elseif ($bins) {
    Write-Host "Using cached gfx1201 OpenCL binary."
}

Write-Host "Starting poworker (WG=64 US=64)..."
$out = Join-Path $Release "mining-live.out.log"
$err = Join-Path $Release "mining-live.err.log"
Start-Process -FilePath $poworker -WorkingDirectory $Release -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden
Start-Sleep -Seconds 20

if (Test-Path (Join-Path $Release "miner-stats.json")) {
    Get-Content (Join-Path $Release "miner-stats.json") -Raw | ConvertFrom-Json |
        Select-Object status, hashrate_display, effective_work_groups, configured_work_groups, gpu_hashrate_display, active_cpu_threads |
        Format-List
}
Get-Content $err -Tail 5 -ErrorAction SilentlyContinue
Get-Content $out -Tail 8 -ErrorAction SilentlyContinue
Write-Host "Done. Logs: mining-live.out.log / mining-live.err.log"