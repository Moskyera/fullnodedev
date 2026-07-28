<#
================================================================================
HBIT LOCAL CHAIN RIG

Builds a private (non-mainnet) Hacash chain, mines it with the GPU, and stops
when the chain reaches a difficulty the HBIT pool will actually serve, i.e. a
network target with >= 34 leading zero bits.

    PS> .\Run-Rig.ps1 -RewardAddress 1YourPrivakeyAddress

WHY IT WORKS  (the long version is in node.ini.template)
--------------------------------------------------------------------------------
The pool needs >= 34 network bits; a fresh non-mainnet chain starts at 22
because off mainnet ASERT anchors at difficulty_adjust_blocks+2 with the fixed
constant 0xe9cfffff. ASERT is an equilibrium controller, so the resting
difficulty of a private chain is log2(hashrate * each_block_target_time). Set
each_block_target_time and you have set the difficulty. That is the entire
trick, and it is a config change only: no consensus code is touched, so mainnet
behaviour is byte-identical by construction.

MEASURED, NOT ESTIMATED
--------------------------------------------------------------------------------
One full run of this script, AMD RX 9070 XT at 294-307 MH/s (x16rs repeat=1),
-TargetTimeSecs 455, from genesis. Times are the chain's own block timestamps
relative to height 1, so they are reproducible from the chain afterwards:

    22 bits   height  14   t+    5s     (the ASERT anchor, fixed constant)
    28 bits   height 146   t+  132s
    31 bits   height 218   t+  414s
    33 bits   height 267   t+ 1122s
    34 bits   height 294   t+ 2517s     <- pool servable
    35 bits   height 321   t+ 4156s

Wall clock ran about 5% longer than chain time throughout (44m05s to 34 bits).
That is not drift: a block's stored timestamp is when its TEMPLATE was built,
not when it was solved, so at high difficulty the chain's clock trails the
operator's by roughly one block interval.

At 34 bits `hbit-pool-server <node> <key> 127.0.0.1:9788 18 testnet:8:455`
started, verified itself against the node and served: /terms reported
share_factor_achieved 18, share_cost_bits 16, crediting_shares true.
`hbit-asert-check ... testnet:8:455` matched the chain's own stored difficulty
on every block checked, twice (at 30 bits and at 34).

Two things that run showed which the arithmetic does not:

  * HEIGHT is predictable, WALL CLOCK is not. The planner said height 292; the
    chain did 294. The planner said 26m26s; it took 44m05s. Blocks above 32 bits
    landed at roughly half the rate the raw hashrate implies. Trust the height,
    and budget about 1.7x the planner's time.

  * 34 bits is the EDGE, not a target. At exactly 34, share_bits 18 leaves
    achieved 18 and cost 16, both sitting on their minimums; share_bits 20 and
    24 are refused outright, and one bit of difficulty drop halts crediting.
    That is why -WantBits defaults to 35. If you want the two bits of headroom
    that share_bits 20 needs, do NOT wait longer at 455 (35 -> 36 doubles again);
    raise -TargetTimeSecs to 910, which the planner puts at an equilibrium of 38
    bits and reaching 34 in a quarter of the time. That variant is MODELLED, not
    measured: the run above was 455.

WHAT IT DOES NOT DO
--------------------------------------------------------------------------------
It does not touch mainnet, it does not connect to peers (not_find_nodes = true),
and it does not use, create or unlock any wallet. -RewardAddress is an address
only: block rewards are paid to it and nothing in this rig can spend them. To
run the POOL against this chain afterwards you supply the pool's own wallet
separately, as documented in docs/POOL-OPERATOR.md.

EXIT CODE
--------------------------------------------------------------------------------
0 = the chain reached -WantBits inside -DeadlineSecs.
1 = it did not. The per-bit timeline is printed either way.
2 = setup failed (build, missing exe, node never came up).
Check the exit code. The log text is not the result.
================================================================================
#>

[CmdletBinding()]
param(
    # Address that block rewards go to. Must be a PRIVAKEY address ("1...") or
    # the node refuses to start. No key is needed or used.
    [Parameter(Mandatory = $true)]
    [string] $RewardAddress,

    # [mint] each_block_target_time. THE knob. Run the planner to pick it:
    #   cargo run --release -p hbit-pool --example testnet_rig_plan -- <MH/s>
    # 455 puts the equilibrium at ~37 bits for ~300 MH/s at x16rs repeat=1.
    [int] $TargetTimeSecs = 455,

    # Stop as soon as the chain reaches this many network leading-zero bits.
    # 34 is the pool's hard FLOOR and leaves no margin at all (see MEASURED
    # above), so the default is one bit above it. The chain keeps climbing after
    # the script stops, so margin grows on its own if you leave the node up.
    [int] $WantBits = 35,

    # Give up after this long. A measured run reached 34 bits at t+2517s and 35
    # at t+4156s of chain time, so this is a bit over 2x the time to the default.
    [int] $DeadlineSecs = 9000,

    [int] $RpcPort = 8099,
    [int] $P2pPort = 3099,

    # Where the chain lives. Wiped on every run unless -Resume.
    [string] $DataDir = "$env:LOCALAPPDATA\hbit-local-chain-rig\data",

    # Keep the existing chain instead of starting from genesis.
    [switch] $Resume,

    # Leave the node and worker running after the watch finishes, so the pool
    # can be pointed at the chain while it is still in the band.
    [switch] $KeepRunning,

    # Skip cargo build (they are already built).
    [switch] $NoBuild
)

$ErrorActionPreference = 'Stop'
$rigDir  = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo    = Resolve-Path (Join-Path $rigDir '..\..')
$rel     = Join-Path $repo 'target\release'
$runDir  = Join-Path $rigDir 'run'
$fullnodeExe = Join-Path $rel 'fullnode.exe'
$poworkerExe = Join-Path $rel 'poworker.exe'
$watchExe    = Join-Path $rel 'examples\local_chain_watch.exe'

function Fail($msg) { Write-Host "[rig] SETUP FAILED: $msg" -ForegroundColor Red; exit 2 }

Write-Host "[rig] repo      = $repo"
Write-Host "[rig] data_dir  = $DataDir"
Write-Host "[rig] each_block_target_time = $TargetTimeSecs s"
Write-Host "[rig] want      >= $WantBits network bits, deadline $DeadlineSecs s"

# ---------------------------------------------------------------- build -------
if (-not $NoBuild) {
    Write-Host "[rig] building fullnode + poworker (ocl) ..."
    Push-Location $repo
    cargo build --release --features ocl --bin fullnode --bin poworker
    $rc = $LASTEXITCODE
    Pop-Location
    if ($rc -ne 0) { Fail "cargo build --features ocl exited $rc" }

    Write-Host "[rig] building the watch instrument ..."
    Push-Location $repo
    cargo build --release -p hbit-pool --example local_chain_watch
    $rc = $LASTEXITCODE
    Pop-Location
    if ($rc -ne 0) { Fail "cargo build --example local_chain_watch exited $rc" }
}
foreach ($e in @($fullnodeExe, $poworkerExe, $watchExe)) {
    if (-not (Test-Path $e)) { Fail "missing $e (drop -NoBuild)" }
}

# --------------------------------------------------------------- config -------
if (Test-Path $runDir) { Remove-Item -Recurse -Force $runDir }
New-Item -ItemType Directory -Force -Path $runDir | Out-Null

if (-not $Resume) {
    if (Test-Path $DataDir) {
        Write-Host "[rig] wiping $DataDir for a chain from genesis"
        Remove-Item -Recurse -Force $DataDir
    }
}
New-Item -ItemType Directory -Force -Path $DataDir | Out-Null

$nodeIni = Join-Path $runDir 'node.ini'
(Get-Content (Join-Path $rigDir 'node.ini.template') -Raw).
    Replace('@@DATA_DIR@@',       ($DataDir -replace '\\', '/')).
    Replace('@@P2P_PORT@@',       "$P2pPort").
    Replace('@@RPC_PORT@@',       "$RpcPort").
    Replace('@@TARGET_TIME@@',    "$TargetTimeSecs").
    Replace('@@REWARD_ADDRESS@@', $RewardAddress) |
    Set-Content -Encoding utf8 $nodeIni

$powIni = Join-Path $runDir 'poworker.ini'
(Get-Content (Join-Path $rigDir 'poworker.ini.template') -Raw).
    Replace('@@RPC_HOST_PORT@@', "127.0.0.1:$RpcPort").
    Replace('@@OPENCL_DIR@@',    ((Join-Path $repo 'x16rs\opencl') -replace '\\', '/') + '/').
    Replace('@@STATS_FILE@@',    ((Join-Path $runDir 'miner-stats.json') -replace '\\', '/')) |
    Set-Content -Encoding utf8 $powIni

Write-Host "[rig] rendered $nodeIni"
Write-Host "[rig] rendered $powIni"

# ---------------------------------------------------------------- start -------
$started = @()
function Stop-Started {
    foreach ($p in $started) {
        if ($p -and -not $p.HasExited) {
            Write-Host "[rig] stopping pid $($p.Id) ($($p.ProcessName))"
            try { Stop-Process -Id $p.Id -Force -ErrorAction Stop } catch {}
        }
    }
}

$nodeOut = Join-Path $runDir 'fullnode.out.log'
$nodeErr = Join-Path $runDir 'fullnode.err.log'
$node = Start-Process -FilePath $fullnodeExe -ArgumentList "`"$nodeIni`"" `
    -WorkingDirectory $runDir -RedirectStandardOutput $nodeOut `
    -RedirectStandardError $nodeErr -PassThru -WindowStyle Hidden
$started += $node
Write-Host "[rig] fullnode pid $($node.Id), logs in $runDir"

# Wait for the RPC to answer. A node that died on a config error must not be
# mistaken for a node that is still warming up, so the process is checked too.
$base = "http://127.0.0.1:$RpcPort"
$up = $false
for ($i = 0; $i -lt 60; $i++) {
    if ($node.HasExited) {
        Write-Host (Get-Content $nodeErr -Raw -ErrorAction SilentlyContinue)
        Write-Host (Get-Content $nodeOut -Tail 40 -ErrorAction SilentlyContinue)
        Fail "fullnode exited during startup with code $($node.ExitCode)"
    }
    try {
        Invoke-RestMethod -Uri "$base/query/latest" -TimeoutSec 3 | Out-Null
        $up = $true
        break
    } catch {
        Start-Sleep -Seconds 1
    }
}
if (-not $up) { Stop-Started; Fail "node RPC on $base never answered" }
Write-Host "[rig] node RPC up on $base"

$powOut = Join-Path $runDir 'poworker.out.log'
$powErr = Join-Path $runDir 'poworker.err.log'
$pow = Start-Process -FilePath $poworkerExe -ArgumentList "`"$powIni`"" `
    -WorkingDirectory $runDir -RedirectStandardOutput $powOut `
    -RedirectStandardError $powErr -PassThru -WindowStyle Hidden
$started += $pow
Write-Host "[rig] poworker pid $($pow.Id)"

# poworker exits 0 even when its GPU never initialises, so "it is still alive"
# is the only usable liveness signal here.
Start-Sleep -Seconds 8
if ($pow.HasExited) {
    Write-Host (Get-Content $powOut -Tail 40 -ErrorAction SilentlyContinue)
    Stop-Started
    Fail "poworker exited immediately (exit $($pow.ExitCode)); see $powOut"
}

# ---------------------------------------------------------------- watch -------
Write-Host "[rig] watching the chain climb ...`n"
& $watchExe $base $WantBits $DeadlineSecs 5
$watchRc = $LASTEXITCODE

Write-Host ""
Write-Host "[rig] node config : $nodeIni"
Write-Host "[rig] chain data  : $DataDir"
Write-Host "[rig] pool chain selector for this chain: testnet:8:$TargetTimeSecs"
Write-Host "[rig] prove it before pointing the pool at it:"
Write-Host "[rig]   target\release\hbit-asert-check.exe $base 20 testnet:8:$TargetTimeSecs"

if ($KeepRunning) {
    Write-Host "[rig] leaving fullnode (pid $($node.Id)) and poworker (pid $($pow.Id)) running."
    Write-Host "[rig] stop them with: Stop-Process -Id $($node.Id),$($pow.Id) -Force"
} else {
    Stop-Started
}
exit $watchRc
