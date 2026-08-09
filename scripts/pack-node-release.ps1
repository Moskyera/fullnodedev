param(
    [string]$Version = "manual",
    [string]$OutDir = "dist-node"
)

$ErrorActionPreference = "Stop"
$Root = Split-Path $PSScriptRoot -Parent
$Release = Join-Path $Root "target\release"
$Binary = Join-Path $Release "hacash.exe"
$Config = Join-Path $Root "mainnet-configs\hacash.config.mainnet.ini"
$Readme = Join-Path $Root "README-NODE.txt"
$PackageName = "hpay-compatible-hacash-fullnode-windows-x64"
$Stage = Join-Path $OutDir $PackageName

foreach ($required in @($Binary, $Config, $Readme)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        throw "Missing required node release file: $required"
    }
}

$sourceCommit = (& git -C $Root rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $sourceCommit -notmatch '^[0-9a-f]{40}$') {
    throw "Unable to record an exact 40-character source commit"
}

if (Test-Path -LiteralPath $Stage) {
    Remove-Item -LiteralPath $Stage -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $Stage | Out-Null

Copy-Item -LiteralPath $Binary -Destination (Join-Path $Stage "hacash.exe")
Copy-Item -LiteralPath $Config -Destination (Join-Path $Stage "hacash.config.ini.example")
Copy-Item -LiteralPath $Readme -Destination (Join-Path $Stage "README.txt")
Set-Content -LiteralPath (Join-Path $Stage "VERSION.txt") -Value $Version -NoNewline
Set-Content -LiteralPath (Join-Path $Stage "SOURCE-COMMIT.txt") -Value $sourceCommit -NoNewline

$forbidden = @(
    "poworker.exe", "diaworker.exe", "miner-panel.exe", "hac-pool.exe",
    "hbit-pool-server.exe", "hbit-pool-payout.exe"
)
foreach ($name in $forbidden) {
    if (Test-Path -LiteralPath (Join-Path $Stage $name)) {
        throw "Standalone node package must not contain $name"
    }
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$archive = Join-Path $OutDir "$PackageName-$Version.zip"
if (Test-Path -LiteralPath $archive) {
    Remove-Item -LiteralPath $archive -Force
}
Compress-Archive -LiteralPath $Stage -DestinationPath $archive -CompressionLevel Optimal

$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
$checksum = "$hash  $([IO.Path]::GetFileName($archive))$([Environment]::NewLine)"
$utf8NoBom = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText("$archive.sha256", $checksum, $utf8NoBom)

Write-Host "Packaged standalone node: $archive"

