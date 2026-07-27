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

# Ensure smoke scripts are present
$need = @(
    "scripts\mining-nvidia\colab_cuda_smoke.sh",
    "scripts\mining-nvidia\COLAB-T4.md",
    "x16rs-cuda\Cargo.toml",
    "Cargo.toml"
)
foreach ($rel in $need) {
    $p = Join-Path $Stage $rel
    if (-not (Test-Path $p)) {
        throw "Missing required path in slim pack: $rel"
    }
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
Write-Host "  5) !bash scripts/mining-nvidia/colab_cuda_smoke.sh"
