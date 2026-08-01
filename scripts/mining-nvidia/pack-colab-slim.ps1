# Build a small zip for Google Colab CUDA smoke (no target/, no dist/, no chain data).
# Usage (PowerShell, from anywhere):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\mining-nvidia\pack-colab-slim.ps1
#
# Output: scripts\mining-nvidia\colab-upload\hacash-fullnodedev-colab-slim.zip  (typically tens of MB)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$OutDir = Join-Path $PSScriptRoot "colab-upload"
$Stage = Join-Path $OutDir "hacash-fullnodedev"
$Zip = Join-Path $OutDir "hacash-fullnodedev-colab-slim.zip"

Write-Host "Repo: $Root"
Write-Host "Staging slim tree (source only)..."

if (Test-Path $OutDir) {
    Remove-Item -Recurse -Force $OutDir
}
New-Item -ItemType Directory -Path $Stage -Force | Out-Null

# Exclude heavy / local-only paths
$excludeDirNames = @(
    "target",
    "target2",
    ".git",
    ".tools",
    "colab-upload",
    "colab-results",
    "node_modules",
    "hacash_mainnet_data",
    "hacash_data"
)

# Also skip known dist / codex audit dumps at repo root
$excludeRootPrefixes = @(
    "dist-",
    ".codex-",
    ".final-test",
    "dist"
)

function ShouldSkipDir([System.IO.DirectoryInfo]$dir, [string]$root) {
    $name = $dir.Name
    if ($excludeDirNames -contains $name) { return $true }
    $rel = $dir.FullName.Substring($root.Length).TrimStart('\', '/')
    if ($rel -notmatch '[\\/]') {
        foreach ($p in $excludeRootPrefixes) {
            if ($name.StartsWith($p, [StringComparison]::OrdinalIgnoreCase)) { return $true }
        }
    }
    return $false
}

function Copy-Slim([string]$src, [string]$dstRoot, [string]$repoRoot) {
    $srcItem = Get-Item -LiteralPath $src -Force -ErrorAction SilentlyContinue
    if (-not $srcItem) { return }
    # Skip reparse points / broken links (e.g. odd .git states)
    if ($srcItem.Attributes -band [IO.FileAttributes]::ReparsePoint) { return }

    if ($srcItem.PSIsContainer) {
        if (ShouldSkipDir $srcItem $repoRoot) { return }
        $dest = Join-Path $dstRoot $srcItem.Name
        New-Item -ItemType Directory -Path $dest -Force | Out-Null
        Get-ChildItem -LiteralPath $srcItem.FullName -Force -ErrorAction SilentlyContinue | ForEach-Object {
            Copy-Slim $_.FullName $dest $repoRoot
        }
    } else {
        # skip huge logs / binaries
        $ext = $srcItem.Extension.ToLowerInvariant()
        if ($ext -in @(".exe", ".dll", ".pdb", ".rlib", ".rmeta")) { return }
        if ($srcItem.Length -gt 50MB) {
            Write-Host "  skip large file: $($srcItem.Name) ($([math]::Round($srcItem.Length/1MB,1)) MB)"
            return
        }
        Copy-Item -LiteralPath $srcItem.FullName -Destination (Join-Path $dstRoot $srcItem.Name) -Force
    }
}

Get-ChildItem -LiteralPath $Root -Force -ErrorAction SilentlyContinue | ForEach-Object {
    # Skip .git entirely (not needed for Colab smoke)
    if ($_.Name -eq ".git") { return }
    if ($excludeDirNames -contains $_.Name) { return }
    if ($_.PSIsContainer) {
        $skipRoot = $false
        foreach ($p in $excludeRootPrefixes) {
            if ($_.Name.StartsWith($p, [StringComparison]::OrdinalIgnoreCase)) { $skipRoot = $true; break }
        }
        if ($skipRoot) { return }
    }
    Copy-Slim $_.FullName $Stage $Root
}

# Ensure the scripts are present. The zip carries no .git, so nothing downstream
# can tell what commit it came from; a pack that is quietly missing the gate would
# reach Colab as a build that measures speed and proves nothing.
$need = @(
    "scripts\mining-nvidia\colab_cuda_smoke.sh",
    "scripts\mining-nvidia\colab_cuda_gate.sh",
    "scripts\mining-nvidia\COLAB-T4.md",
    "scripts\x16rs_gate_trees.py",
    "src\bin\x16rs_gate.rs",
    "app\src\x16rs_gate.rs",
    "x16rs-cuda\build.rs",
    "x16rs-cuda\cuda\block_miner.cu",
    "x16rs\opencl\x16rs.cl",
    "x16rs-cuda\Cargo.toml",
    "Cargo.toml"
)
foreach ($rel in $need) {
    $p = Join-Path $Stage $rel
    if (-not (Test-Path $p)) {
        throw "Missing required path in slim pack: $rel"
    }
}

# The same content checks the Colab notebook makes on a clone, made here instead,
# because a zip has no commit to check.
$content = @{
    "app\src\x16rs_gate.rs"  = "CudaBackend"
    "src\bin\x16rs_gate.rs"  = "--backend"
    "x16rs-cuda\build.rs"    = "X16RS_CUDA_KERNEL_DIR"
    "x16rs\opencl\x16rs.cl"  = "X16RS_H_BLAKE_INIT"
}
foreach ($rel in $content.Keys) {
    $needle = $content[$rel]
    if (-not (Select-String -Path (Join-Path $Stage $rel) -SimpleMatch -Pattern $needle -Quiet)) {
        throw "This tree predates the CUDA gate: $rel does not contain '$needle'. Packing it would ship a Colab run that cannot prove anything."
    }
}

# Stamp the pack so a zip on Colab can still say where it came from. The zip
# carries no .git, so this file is the only thing that identifies the source.
#
# Each value is captured into its own variable first. Calling a native command
# inside an array literal makes PowerShell fold the whole literal into one
# space-joined string, which produced a one-line stamp that looked fine in the
# console and was unreadable as key=value.
$packedUtc = (Get-Date).ToUniversalTime().ToString("yyyyMMddTHHmmssZ")
$commit    = (& git -C $Root rev-parse HEAD) | Select-Object -First 1
$branch    = (& git -C $Root rev-parse --abbrev-ref HEAD) | Select-Object -First 1
$porcelain = & git -C $Root status --porcelain
$dirty     = if ($porcelain) { "true" } else { "false" }
if (-not $commit) { $commit = "unknown" }
if (-not $branch) { $branch = "unknown" }

$stampLines = @(
    "packed_utc=$packedUtc",
    "commit=$commit",
    "branch=$branch",
    "dirty=$dirty"
)
Set-Content -Path (Join-Path $Stage "COLAB-PACK-STAMP.txt") -Value $stampLines -Encoding utf8
Write-Host "Pack stamp:"
foreach ($line in $stampLines) { Write-Host "  $line" }
if ($dirty -eq "true") {
    Write-Host "  NOTE: the working tree has uncommitted changes, so this zip is NOT commit $commit."
    Write-Host "        It is that commit plus whatever is currently uncommitted. Say so in the log."
}

if (Test-Path $Zip) { Remove-Item -Force $Zip }
Write-Host "Compressing..."
Compress-Archive -Path $Stage -DestinationPath $Zip -CompressionLevel Optimal

$zipSize = (Get-Item $Zip).Length
Write-Host ""
Write-Host "OK: $Zip"
Write-Host ("Size: {0:N1} MB" -f ($zipSize / 1MB))
Write-Host ""
Write-Host "Colab:"
Write-Host "  1) Runtime -> T4 GPU"
Write-Host "  2) Upload this zip"
Write-Host "  3) !unzip -q hacash-fullnodedev-colab-slim.zip -d /content"
Write-Host "  4) %cd /content/hacash-fullnodedev"
Write-Host "  5) !bash scripts/mining-nvidia/colab_cuda_gate.sh    # correctness FIRST"
Write-Host "  6) !bash scripts/mining-nvidia/colab_cuda_smoke.sh   # then the crate tests"
Write-Host ""
Write-Host "This zip has no .git, so the gate log will say commit=not-a-git-checkout."
Write-Host "COLAB-PACK-STAMP.txt inside the zip carries the commit instead. Keep them together."
