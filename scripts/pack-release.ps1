# Packages Windows miner release ZIPs from target\release.
# Produces TWO downloads:
#   - hacash-miner-only-*  (workers + panel; you already have fullnode)
#   - hacash-miner-full-*  (fullnode + workers + panel; clean PC)
#
# What is deliberately NOT in either download: the HBIT pool. hbit-pool-server
# and hbit-pool-payout are the author's own infrastructure, run next to the
# author's own fullnode, and they are not published. The miner merely LISTS HBIT
# as a pool it can connect to. Nothing here may start shipping a wallet-holding
# daemon to people who came for a miner; the release workflow asserts their
# absence from every archive.
param(
    [string]$Version = "dev",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
$Release = Join-Path $Root "target\release"
$opencl = Join-Path $Root "x16rs\opencl"
$miningAssets = Join-Path $Root "scripts\mining-amd"
$presets = Join-Path $miningAssets "presets"
$mainnetConfigs = Join-Path $Root "mainnet-configs"
# Templates the docs tell operators to copy. Missing ones must fail the build,
# not ship a package that points at files the user never received.
$requiredMainnetConfigs = @(
    "hacash.config.mainnet.ini",
    "poworker.mainnet.ini",
    "diaworker.mainnet.ini",
    "MAINNET-DIAMOND.md"
)
$requiredKernels = @(
    "aes_helper.cl", "blake.cl", "bmw.cl", "cubehash.cl", "echo.cl",
    "fugue.cl", "groestl.cl", "hamsi.cl", "hamsi_help.cl",
    "hamsi_helper.cl", "hamsi_helper_big.cl", "jh.cl", "keccak.cl",
    "luffa.cl", "sha2_512.cl", "sha3_256.cl", "shabal.cl", "shavite.cl",
    "simd.cl", "skein.cl", "util.cl", "whirlpool.cl", "x16rs.cl",
    "x16rs_diamond.cl", "x16rs_main.cl"
)

if (-not (Test-Path $Release)) {
    throw "Missing folder: $Release - run cargo build first."
}
foreach ($kernel in $requiredKernels) {
    $kernelPath = Join-Path $opencl $kernel
    if (-not (Test-Path -LiteralPath $kernelPath -PathType Leaf)) {
        throw "Missing required OpenCL kernel: $kernelPath"
    }
}

foreach ($required in @(
    (Join-Path $miningAssets "poworker.amd.ini.example"),
    (Join-Path $miningAssets "diaworker.amd.ini.example"),
    (Join-Path $miningAssets "PRESETS-INDEX.txt")
)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Missing required mining asset: $required"
    }
}
if (-not (Test-Path -LiteralPath $presets -PathType Container)) {
    throw "Missing presets folder: $presets"
}

if (-not (Test-Path -LiteralPath $mainnetConfigs -PathType Container)) {
    throw "Missing mainnet-configs folder: $mainnetConfigs"
}
foreach ($name in $requiredMainnetConfigs) {
    $templatePath = Join-Path $mainnetConfigs $name
    if (-not (Test-Path -LiteralPath $templatePath -PathType Leaf)) {
        throw "Missing required mainnet template: $templatePath"
    }
}
# A shipped template must never carry a live reward address or bid password: a
# user who copies it as instructed would pay every block reward to whoever owns
# that address, permanently and with no error. Only a commented placeholder is
# allowed, so the node fails loudly until the operator supplies their own.
foreach ($template in @(Get-ChildItem -LiteralPath $mainnetConfigs -Filter "*.ini" -File)) {
    $offending = @(
        Get-Content -LiteralPath $template.FullName |
            Where-Object { $_ -match '^\s*(reward|bid_password)\s*=\s*\S' }
    )
    if ($offending.Count -gt 0) {
        throw "$($template.Name) ships a pre-filled reward/bid_password: $($offending -join '; ')"
    }
}

$minerOnlyExes = @(
    "poworker.exe",
    "diaworker.exe",
    "list_opencl.exe",
    "diagnose_opencl.exe",
    "miner-panel.exe"
)
# Optional public pool binary (all-in-one panel host)
$optionalExes = @("hac-pool.exe")
$fullExes = @("hacash.exe") + $minerOnlyExes

foreach ($e in $fullExes) {
    if (-not (Test-Path (Join-Path $Release $e))) {
        throw "Missing binary: $(Join-Path $Release $e)"
    }
}
foreach ($e in $optionalExes) {
    if (-not (Test-Path (Join-Path $Release $e))) {
        Write-Warning "Optional binary missing (public pool UI needs it): $(Join-Path $Release $e)"
    }
}

function Copy-OpenClKernels {
    param([string]$Stage)
    $oclDest = Join-Path $Stage "x16rs\opencl"
    New-Item -ItemType Directory -Force -Path $oclDest | Out-Null
    Get-ChildItem $opencl -Filter "*.cl" | Copy-Item -Destination $oclDest
}

function Copy-Logo {
    param([string]$Stage)
    $logo = Join-Path $Root "miner-panel\assets\hhh.png"
    if (Test-Path $logo) {
        Copy-Item $logo (Join-Path $Stage "hhh.png")
    }
}

function Copy-MiningAssets {
    param([string]$Stage)

    foreach ($name in @("poworker.amd.ini.example", "diaworker.amd.ini.example")) {
        Copy-Item -LiteralPath (Join-Path $miningAssets $name) -Destination (Join-Path $Stage $name)
    }
    Copy-Item -LiteralPath (Join-Path $miningAssets "PRESETS-INDEX.txt") -Destination (Join-Path $Stage "PRESETS-INDEX.txt")

    $presetsDest = Join-Path $Stage "presets"
    New-Item -ItemType Directory -Force -Path $presetsDest | Out-Null
    Get-ChildItem -LiteralPath $presets | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $presetsDest -Recurse -Force
    }

    foreach ($kind in @("poworker", "diaworker")) {
        $kindPath = Join-Path $presetsDest $kind
        $count = @(Get-ChildItem -LiteralPath $kindPath -Filter "*.ini" -File).Count
        if ($count -ne 23) {
            throw "Expected 23 $kind presets in $kindPath, found $count"
        }
    }
}

function Copy-MainnetConfigs {
    param([string]$Stage)

    $dest = Join-Path $Stage "mainnet-configs"
    New-Item -ItemType Directory -Force -Path $dest | Out-Null
    Get-ChildItem -LiteralPath $mainnetConfigs -File | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination $dest -Force
    }

    foreach ($name in $requiredMainnetConfigs) {
        if (-not (Test-Path -LiteralPath (Join-Path $dest $name) -PathType Leaf)) {
            throw "Staged package is missing mainnet-configs\$name"
        }
    }
}

function Write-Sha256 {
    param([string]$Path)

    $resolved = (Resolve-Path -LiteralPath $Path).Path
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $resolved).Hash.ToLowerInvariant()
    $line = "$hash  $([System.IO.Path]::GetFileName($resolved))$([Environment]::NewLine)"
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText("$resolved.sha256", $line, $utf8NoBom)
}

function Pack-Flavor {
    param(
        [string]$PackageName,
        [string[]]$Exes,
        [string[]]$Extras,
        [string]$Version
    )

    foreach ($f in $Extras) {
        $src = Join-Path $Root $f
        if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
            throw "Missing required release file: $src"
        }
    }

    $Stage = Join-Path $OutDir $PackageName
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $Stage | Out-Null

    foreach ($e in $Exes) {
        Copy-Item (Join-Path $Release $e) (Join-Path $Stage $e)
    }
    # Ship public pool when built (panel all-in-one host)
    $hacPool = Join-Path $Release "hac-pool.exe"
    if (Test-Path -LiteralPath $hacPool -PathType Leaf) {
        Copy-Item -LiteralPath $hacPool -Destination (Join-Path $Stage "hac-pool.exe")
    }
    Copy-OpenClKernels $Stage
    Copy-Logo $Stage
    Copy-MiningAssets $Stage
    Copy-MainnetConfigs $Stage

    foreach ($f in $Extras) {
        $src = Join-Path $Root $f
        Copy-Item -LiteralPath $src -Destination (Join-Path $Stage $f)
    }

    # The download is the MINER. The HBIT pool is the author's own
    # infrastructure and is never published, so no route through this function
    # may leave a pool file staged: a wallet-holding daemon reaching people who
    # came for a miner is the failure this asserts against.
    foreach ($name in @(
        "hbit-pool-server.exe", "hbit-pool-payout.exe",
        "hbit-pool.example.ini", "POOL-OPERATOR.md", "README-POOL.txt"
    )) {
        if (Test-Path -LiteralPath (Join-Path $Stage $name)) {
            throw "$PackageName must not contain the pool file $name"
        }
    }

    Set-Content -Path (Join-Path $Stage "VERSION.txt") -Value $Version -NoNewline

    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    $zipName = if ($Version -match "^v") {
        "$PackageName-$Version.zip"
    } else {
        "$PackageName.zip"
    }
    $zipPath = Join-Path $OutDir $zipName
    if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
    Compress-Archive -Path $Stage -DestinationPath $zipPath -CompressionLevel Optimal
    Write-Sha256 $zipPath
    return $zipPath
}

$common = @("START-MINER-PANEL.bat", "LIST-OPENCL.bat")

$zipMiner = Pack-Flavor `
    -PackageName "hacash-miner-only-windows-x64" `
    -Exes $minerOnlyExes `
    -Extras ($common + @("SETUP-MINER.bat", "README-MINER-ONLY.txt")) `
    -Version $Version

$zipFull = Pack-Flavor `
    -PackageName "hacash-miner-full-windows-x64" `
    -Exes $fullExes `
    -Extras ($common + @("SETUP.bat", "README-RELEASE.txt")) `
    -Version $Version

Write-Host ""
Write-Host "  Packaged (miner only): $zipMiner"
Write-Host "  Packaged (full stack): $zipFull"
Write-Host ""