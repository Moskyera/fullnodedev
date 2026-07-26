# Packages Windows miner release ZIPs from target\release.
# Produces TWO downloads:
#   - hacash-miner-only-*  (workers + panel; you already have fullnode)
#   - hacash-miner-full-*  (fullnode + workers + panel + HBIT pool; clean PC)
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
# HBIT pool operator files. The pool holds real miner money, so the runbook and
# the argument worksheet ship WITH the binaries: a runbook on a web page the
# operator never opens is the same as no runbook.
$poolRunbook = Join-Path $Root "docs\POOL-OPERATOR.md"
$poolWorksheet = Join-Path $Root "hbit-pool\hbit-pool.example.ini"
$poolReadme = Join-Path $Root "README-POOL.txt"
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

foreach ($required in @($poolRunbook, $poolWorksheet, $poolReadme)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        $hint = ""
        if ($required -eq $poolWorksheet) {
            # .gitignore blanket-ignores *.ini and needs one explicit negation
            # per shipped template, the way it already carries one for the
            # sibling pool. Without that line the worksheet exists on the
            # author's disk but never reaches a clone or a CI checkout, and this
            # is the only place that says so.
            $hint = ". A checkout missing only this file usually means .gitignore lacks the line" +
                " '!hbit-pool/hbit-pool.example.ini', which belongs next to the existing" +
                " '!miner-pool/hac-pool.example.ini'"
        }
        throw "Missing required HBIT pool file: $required$hint"
    }
}
# The worksheet documents the ARGUMENTS hbit-pool-server takes; it must ship
# with every operator-specific answer BLANK. A shipped node URL, wallet path,
# listen address or chain is a value somebody pastes without reading, and one of
# them decides which wallet holds other people's mining income.
foreach ($field in @("node", "wallet_file", "listen", "chain", "pool_base")) {
    $filled = @(
        Get-Content -LiteralPath $poolWorksheet |
            Where-Object { $_ -match "^\s*$field\s*=\s*\S" }
    )
    if ($filled.Count -gt 0) {
        throw "hbit-pool.example.ini ships a filled-in '$field': $($filled -join '; ')"
    }
}
# Nothing an operator receives may carry a live address, private key or
# passphrase. An earlier audit found a shipped template with a valid third-party
# reward address in it, which would have paid a stranger; these two files are
# now scanned for the same class of mistake, comments included.
foreach ($doc in @($poolWorksheet, $poolReadme)) {
    $name = [System.IO.Path]::GetFileName($doc)
    $text = Get-Content -LiteralPath $doc -Raw
    if ($text -match '\b1[1-9A-HJ-NP-Za-km-z]{25,34}\b') {
        throw "$name ships something shaped like a live HAC address: $($Matches[0])"
    }
    if ($text -match '\b[0-9a-fA-F]{64}\b') {
        throw "$name ships something shaped like a private key: $($Matches[0])"
    }
    $secret = @(
        Get-Content -LiteralPath $doc |
            Where-Object { $_ -match '(?i)^\s*(password|passphrase|HBIT_WALLET_PASSWORD\w*)\s*=\s*\S' }
    )
    if ($secret.Count -gt 0) {
        throw "$name must never carry a passphrase value: $($secret -join '; ')"
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
# HBIT payout pool. FULL package only: it needs a synced fullnode of its own to
# fetch templates, submit blocks and settle, and the full package is the one
# that ships that node. The miner-only package is the small worker-rig payload
# for someone who just wants to mine, and a wallet-holding daemon they will
# never start is clutter with a downside.
$poolExes = @(
    "hbit-pool-server.exe",
    "hbit-pool-payout.exe"
)

foreach ($e in $fullExes) {
    if (-not (Test-Path (Join-Path $Release $e))) {
        throw "Missing binary: $(Join-Path $Release $e)"
    }
}
foreach ($e in $poolExes) {
    if (-not (Test-Path (Join-Path $Release $e))) {
        throw "Missing binary: $(Join-Path $Release $e) - build it with: cargo build --release -p hbit-pool"
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

function Copy-PoolAssets {
    param([string]$Stage)

    foreach ($e in $poolExes) {
        Copy-Item -LiteralPath (Join-Path $Release $e) -Destination (Join-Path $Stage $e)
    }
    # The runbook and the worksheet travel with the binaries. Shipping the pool
    # without them is what the earlier refusal to package it was about.
    Copy-Item -LiteralPath $poolRunbook -Destination (Join-Path $Stage "POOL-OPERATOR.md")
    Copy-Item -LiteralPath $poolWorksheet -Destination (Join-Path $Stage "hbit-pool.example.ini")

    foreach ($name in @(
        "hbit-pool-server.exe", "hbit-pool-payout.exe",
        "POOL-OPERATOR.md", "hbit-pool.example.ini"
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $Stage $name) -PathType Leaf)) {
            throw "Staged package is missing $name"
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
        [string]$Version,
        [switch]$IncludePool
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
    if ($IncludePool) { Copy-PoolAssets $Stage }

    foreach ($f in $Extras) {
        $src = Join-Path $Root $f
        Copy-Item -LiteralPath $src -Destination (Join-Path $Stage $f)
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
    -Extras ($common + @("SETUP.bat", "README-RELEASE.txt", "README-POOL.txt")) `
    -Version $Version `
    -IncludePool

Write-Host ""
Write-Host "  Packaged (miner only): $zipMiner"
Write-Host "  Packaged (full stack): $zipFull"
Write-Host ""