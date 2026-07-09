# Packages Windows miner release ZIPs from target\release.
# Produces TWO downloads:
#   - hacash-miner-only-*  (workers + panel; you already have fullnode)
#   - hacash-miner-full-*  (fullnode + workers + panel; clean PC)
param(
    [string]$Version = "dev",
    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
$Release = Join-Path $Root "target\release"
$opencl = Join-Path $Root "x16rs\opencl"

if (-not (Test-Path $Release)) {
    throw "Missing folder: $Release — run cargo build first."
}
if (-not (Test-Path (Join-Path $opencl "x16rs_main.cl"))) {
    throw "Missing OpenCL kernels: $opencl"
}

$minerOnlyExes = @(
    "poworker.exe",
    "diaworker.exe",
    "list_opencl.exe",
    "miner-panel.exe"
)
$fullExes = @("hacash.exe") + $minerOnlyExes

foreach ($e in $fullExes) {
    if (-not (Test-Path (Join-Path $Release $e))) {
        throw "Missing binary: $(Join-Path $Release $e)"
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

function Pack-Flavor {
    param(
        [string]$PackageName,
        [string[]]$Exes,
        [string[]]$Extras,
        [string]$Version
    )

    $Stage = Join-Path $OutDir $PackageName
    if (Test-Path $Stage) { Remove-Item $Stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $Stage | Out-Null

    foreach ($e in $Exes) {
        Copy-Item (Join-Path $Release $e) (Join-Path $Stage $e)
    }
    Copy-OpenClKernels $Stage
    Copy-Logo $Stage

    foreach ($f in $Extras) {
        $src = Join-Path $Root $f
        if (Test-Path $src) {
            Copy-Item $src (Join-Path $Stage $f)
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