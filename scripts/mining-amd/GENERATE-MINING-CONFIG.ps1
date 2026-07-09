# Generates poworker.config.ini + diaworker.config.ini from CPU + GPU input.
# Usage: interactive (no args) or  -Cpu "9950x" -Gpu "7900xtx"

param(
    [string]$Cpu = "",
    [string]$Gpu = "",
    [switch]$NoPause
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = Resolve-Path (Join-Path $ScriptDir "..\..")
$PresetsDir = Join-Path $ScriptDir "presets"

$CpuCatalog = @(
    @{ Id = "1";  Slug = "ryzen5";       Label = "Ryzen 5 (5600X, 7600X - 6 cores)";           Supervene = 4;  Aliases = @("ryzen5","r5","5600","5600x","7600","7600x","ryzen 5") }
    @{ Id = "2";  Slug = "ryzen7";       Label = "Ryzen 7 (5800X, 7700X - 8 cores)";           Supervene = 6;  Aliases = @("ryzen7","r7","5800","5800x","7700","7700x","7800x3d","7800","ryzen 7") }
    @{ Id = "3";  Slug = "ryzen9";       Label = "Ryzen 9 (5900X, 7900X - 12 cores)";          Supervene = 8;  Aliases = @("ryzen9","r9","5900","5900x","7900","7900x","ryzen 9") }
    @{ Id = "4";  Slug = "ryzen9-9950x"; Label = "Ryzen 9 9950X (16 cores / Zen 5)";          Supervene = 10; Aliases = @("9950","9950x","ryzen9-9950x","ryzen 9 9950x") }
    @{ Id = "5";  Slug = "tr-7960x";     Label = "Threadripper 7960X (24 cores)";             Supervene = 14; Aliases = @("7960","7960x","tr7960","tr-7960","threadripper-7960","threadripper 7960") }
    @{ Id = "6";  Slug = "tr-7970x";     Label = "Threadripper 7970X (32 cores)";             Supervene = 18; Aliases = @("7970","7970x","tr7970","tr-7970","threadripper-7970") }
    @{ Id = "7";  Slug = "tr-7980x";     Label = "Threadripper 7980X / 9980WX (64+ cores)";   Supervene = 22; Aliases = @("7980","7980x","9980","9980wx","tr7980","tr-7980","threadripper-7980","threadripper 7980") }
    @{ Id = "8";  Slug = "cpu-only";     Label = "CPU only (no AMD GPU)";                     Supervene = 0;  Aliases = @("cpu","cpu-only","no-gpu","nogpu","none","without-gpu","horis-gpu") }
)

$GpuCatalog = @(
    @{ Id = "1";  Slug = "rx6600";    Label = "RX 6600 / 6600 XT (8 GB)";     Profile = "amd_balanced";    WorkGroups = 1024; UnitSize = 128; VramGb = 8;  Aliases = @("6600","6600xt","rx6600","rx 6600") }
    @{ Id = "2";  Slug = "rx7600";    Label = "RX 7600 (8 GB)";                 Profile = "amd_balanced";    WorkGroups = 1024; UnitSize = 128; VramGb = 8;  Aliases = @("7600","rx7600","rx 7600") }
    @{ Id = "3";  Slug = "rx6700xt";  Label = "RX 6700 XT (12 GB)";             Profile = "amd_performance"; WorkGroups = 2048; UnitSize = 96;  VramGb = 12; Aliases = @("6700","6700xt","rx6700","rx6700xt","rx 6700") }
    @{ Id = "4";  Slug = "rx6800xt";  Label = "RX 6800 / 6800 XT (16 GB)";    Profile = "amd_performance"; WorkGroups = 2048; UnitSize = 96;  VramGb = 16; Aliases = @("6800","6800xt","rx6800","rx6800xt","rx 6800") }
    @{ Id = "5";  Slug = "rx6900xt";  Label = "RX 6900 XT (16 GB)";             Profile = "amd_performance"; WorkGroups = 2048; UnitSize = 96;  VramGb = 16; Aliases = @("6900","6900xt","rx6900","rx6900xt","rx 6900") }
    @{ Id = "6";  Slug = "rx7900xt";  Label = "RX 7900 XT / GRE (16-20 GB)";   Profile = "amd_performance"; WorkGroups = 2048; UnitSize = 96;  VramGb = 20; Aliases = @("7900xt","7900gre","7900 gre","rx7900xt","rx 7900xt","7900 xt") }
    @{ Id = "7";  Slug = "rx7900xtx"; Label = "RX 7900 XTX (24 GB)";            Profile = "amd_max";         WorkGroups = 4096; UnitSize = 128; VramGb = 24; Aliases = @("7900xtx","rx7900xtx","rx 7900 xtx","7900 xtx") }
    @{ Id = "8";  Slug = "rx9070xt";  Label = "RX 9070 XT (16 GB, RDNA4)";      Profile = "amd_performance"; WorkGroups = 2048; UnitSize = 96;  VramGb = 16; Aliases = @("9070","9070xt","rx9070","rx9070xt","rx 9070") }
    @{ Id = "9";  Slug = "none";      Label = "(no GPU - CPU only)";              Profile = "";                WorkGroups = 0;    UnitSize = 0;   VramGb = 0;  Aliases = @("none","no-gpu","nogpu","cpu","skip") }
)

function Normalize([string]$s) {
    if (-not $s) { return "" }
    $s = $s.ToLower().Trim()
    $s = $s -replace '\s+', ''
    $s = $s -replace '^rx', ''
    $s = $s -replace 'threadripper', 'tr'
    $s = $s -replace 'ryzen', 'ryzen'
    return $s
}

function Resolve-Entry([string]$Query, $Catalog) {
    $norm = Normalize $Query
    if (-not $norm) { return $null }

    foreach ($item in $Catalog) {
        if ($norm -eq $item.Id) { return $item }
    }

    $best = $null
    $bestScore = 0
    foreach ($item in $Catalog) {
        foreach ($alias in $item.Aliases) {
            $a = Normalize $alias
            if ($norm -eq $a) { return $item }
            if ($norm.Contains($a) -or $a.Contains($norm)) {
                $score = [Math]::Min($norm.Length, $a.Length)
                if ($score -gt $bestScore) { $bestScore = $score; $best = $item }
            }
        }
        $slugNorm = Normalize $item.Slug
        if ($norm -eq $slugNorm -or $norm.Contains($slugNorm)) {
            $score = $slugNorm.Length
            if ($score -gt $bestScore) { $bestScore = $score; $best = $item }
        }
    }
    return $best
}

function Get-PresetSlug($CpuEntry, $GpuEntry) {
    if ($CpuEntry.Slug -eq "cpu-only" -or $GpuEntry.Slug -eq "none") {
        switch ($CpuEntry.Slug) {
            "ryzen5"       { return "cpu-only-ryzen5" }
            "ryzen7"       { return "cpu-only-ryzen7" }
            "ryzen9"       { return "cpu-only-ryzen9" }
            "ryzen9-9950x" { return "cpu-only-ryzen9" }
            "tr-7960x"     { return "cpu-only-ryzen9" }
            "tr-7970x"     { return "cpu-only-ryzen9" }
            "tr-7980x"     { return "cpu-only-ryzen9" }
            "cpu-only"     { return "cpu-only-ryzen7" }
            default        { return "cpu-only-ryzen7" }
        }
    }

    $cpuPart = $CpuEntry.Slug
    $gpuPart = switch ($GpuEntry.Slug) {
        "rx6600"    { "rx6600" }
        "rx7600"    { "rx7600" }
        "rx6700xt"  { "rx6700xt" }
        "rx6800xt"  { "rx6800xt" }
        "rx6900xt"  { "rx6800xt" }
        "rx7900xt"  { "rx7900xt" }
        "rx7900xtx" { "rx7900xtx" }
        "rx9070xt"  { "rx9070xt" }
        default     { $GpuEntry.Slug }
    }
    return "$cpuPart-$gpuPart"
}

function Get-CpuOnlySupervene($CpuEntry) {
    switch ($CpuEntry.Slug) {
        "ryzen5"       { return 10 }
        "ryzen7"       { return 14 }
        "ryzen9"       { return 20 }
        "ryzen9-9950x" { return 20 }
        "tr-7960x"     { return 24 }
        "tr-7970x"     { return 28 }
        "tr-7980x"     { return 30 }
        default        { return 14 }
    }
}

function New-PoworkerContent($CpuEntry, $GpuEntry, [string]$ComboLabel) {
    $cpuOnly = ($CpuEntry.Slug -eq "cpu-only") -or ($GpuEntry.Slug -eq "none")
    $sv = if ($cpuOnly) { Get-CpuOnlySupervene $CpuEntry } else { $CpuEntry.Supervene }

    $lines = @(
        "; ============================================================================"
        "; HAC block miner - auto-generated for: $ComboLabel"
        "; Generated by CONFIGURE-MINING.bat / GENERATE-MINING-CONFIG.ps1"
        "; Set platform_id / device_ids after LIST-OPENCL-DEVICES.bat"
        "; ============================================================================"
        ""
        "connect = 127.0.0.1:8080"
        "supervene = $sv"
        "nonce_max = 4294967295"
        "notice_wait = 45"
        ""
        "[efficiency]"
        "mode = profit"
        "power_cost_kwh = 0.15"
        "gpu_watts = 0"
        "cpu_watts_per_thread = 8"
        "hac_price = 0"
        "dynamic_supervene = true"
        "supervene_min = 2"
        "supervene_max = $sv"
        "oom_fallback = true"
        "max_temp_c = 0"
        "throttle_work_groups = 1024"
        "benchmark_seconds = 0"
        ""
        "[gpu]"
    )

    if ($cpuOnly) {
        $lines += @(
            "use_opencl = false"
            "cpu_assist = false"
            "platform_id = 0"
            "device_ids = 0"
            "opencl_dir = ../../x16rs/opencl/"
            "debug = 0"
        )
    } else {
        $lines += @(
            "use_opencl = true"
            "cpu_assist = true"
            "gpu_profile = $($GpuEntry.Profile)"
            "platform_id = 0"
            "device_ids = 0"
            "opencl_dir = ../../x16rs/opencl/"
            "work_groups = $($GpuEntry.WorkGroups)"
            "local_size = 256"
            "unit_size = $($GpuEntry.UnitSize)"
            "debug = 0"
        )
        if ($GpuEntry.Slug -eq "rx9070xt") {
            $lines += "; Tip: if stable and no OOM, try gpu_profile = amd_max"
        }
    }
    return ($lines -join "`r`n") + "`r`n"
}

function New-DiaworkerContent($CpuEntry, $GpuEntry, [string]$ComboLabel) {
    $cpuOnly = ($CpuEntry.Slug -eq "cpu-only") -or ($GpuEntry.Slug -eq "none")
    $svPow = if ($cpuOnly) { Get-CpuOnlySupervene $CpuEntry } else { $CpuEntry.Supervene }
    $sv = [Math]::Max(2, $svPow - 2)

    $lines = @(
        "; ============================================================================"
        "; HACD diamond miner - auto-generated for: $ComboLabel"
        "; Requires [diamondminer] enable = true in hacash.config.ini"
        "; ============================================================================"
        ""
        "connect = 127.0.0.1:8080"
        "supervene = $sv"
        ""
        "[gpu]"
    )

    if ($cpuOnly) {
        $lines += @(
            "use_opencl = false"
            "cpu_assist = false"
            "platform_id = 0"
            "device_ids = 0"
            "opencl_dir = ../../x16rs/opencl/"
            "debug = 0"
        )
    } else {
        $lines += @(
            "use_opencl = true"
            "cpu_assist = true"
            "gpu_profile = $($GpuEntry.Profile)"
            "platform_id = 0"
            "device_ids = 0"
            "opencl_dir = ../../x16rs/opencl/"
            "work_groups = $($GpuEntry.WorkGroups)"
            "local_size = 256"
            "unit_size = $($GpuEntry.UnitSize)"
            "debug = 0"
        )
    }
    return ($lines -join "`r`n") + "`r`n"
}

function Install-Configs([string]$PowContent, [string]$DiaContent) {
    $utf8 = New-Object System.Text.UTF8Encoding $false
    $targets = @(
        (Join-Path $RepoRoot "target\debug")
        (Join-Path $RepoRoot "target\release")
    )
    $written = @()
    foreach ($dir in $targets) {
        if (-not (Test-Path $dir)) {
            if ($dir -like "*\debug") { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
            else { continue }
        }
        $powPath = Join-Path $dir "poworker.config.ini"
        $diaPath = Join-Path $dir "diaworker.config.ini"
        [System.IO.File]::WriteAllText($powPath, $PowContent, $utf8)
        [System.IO.File]::WriteAllText($diaPath, $DiaContent, $utf8)
        $written += $powPath
        $written += $diaPath
    }
    return $written
}

function Show-Catalog($Catalog, [string]$Title) {
    Write-Host ""
    Write-Host "  $Title" -ForegroundColor Cyan
    foreach ($item in $Catalog) {
        Write-Host ("  {0,2}  {1}" -f $item.Id, $item.Label)
    }
    Write-Host ""
}

# --- Interactive or CLI ---
if (-not $Cpu) {
    Clear-Host
    Write-Host ""
    Write-Host "  ============================================================" -ForegroundColor Yellow
    Write-Host "   HAC Mining - type your CPU + GPU" -ForegroundColor Yellow
    Write-Host "  ============================================================" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "  Write your CPU and GPU (e.g. 9950x + 7900xtx)"
    Write-Host "  Or pick a number from the lists below."
    Write-Host ""

    Show-Catalog $CpuCatalog "CPU options:"
    $Cpu = Read-Host "  CPU (name or number)"
    Write-Host ""
    Show-Catalog $GpuCatalog "GPU options:"
    $Gpu = Read-Host "  GPU (name or number)"
}

$cpuEntry = Resolve-Entry $Cpu $CpuCatalog
$gpuEntry = Resolve-Entry $Gpu $GpuCatalog

if (-not $cpuEntry) {
    Write-Host ""
    Write-Host "  Could not recognize CPU: '$Cpu'" -ForegroundColor Red
    Write-Host "  Examples: 9950x, ryzen7, 7960x, cpu-only"
    if (-not $NoPause) { Read-Host "  Press Enter to exit" }
    exit 1
}

if (-not $gpuEntry -and $cpuEntry.Slug -ne "cpu-only") {
    Write-Host ""
    Write-Host "  Could not recognize GPU: '$Gpu'" -ForegroundColor Red
    Write-Host "  Examples: 7900xtx, 9070xt, 6700xt, none"
    if (-not $NoPause) { Read-Host "  Press Enter to exit" }
    exit 1
}

if ($cpuEntry.Slug -eq "cpu-only") { $gpuEntry = $GpuCatalog | Where-Object { $_.Slug -eq "none" } | Select-Object -First 1 }
if ($gpuEntry.Slug -eq "none" -and $cpuEntry.Slug -ne "cpu-only") {
    Write-Host ""
    Write-Host "  GPU set to none - using CPU-only mode." -ForegroundColor Yellow
}

$comboLabel = "$($cpuEntry.Label) + $($gpuEntry.Label)"
$presetSlug = Get-PresetSlug $cpuEntry $gpuEntry
$presetPow = Join-Path $PresetsDir "poworker\$presetSlug.ini"
$presetDia = Join-Path $PresetsDir "diaworker\$presetSlug.ini"

Write-Host ""
Write-Host "  Matched:" -ForegroundColor Green
Write-Host "    CPU: $($cpuEntry.Label)"
Write-Host "    GPU: $($gpuEntry.Label)"
if (-not ($cpuEntry.Slug -eq "cpu-only" -or $gpuEntry.Slug -eq "none")) {
    Write-Host "    supervene: $($cpuEntry.Supervene)   gpu_profile: $($gpuEntry.Profile)"
}
Write-Host "    preset: $presetSlug"
Write-Host ""

if ((Test-Path $presetPow) -and (Test-Path $presetDia)) {
    Write-Host "  Using tuned preset file: presets\$presetSlug.ini" -ForegroundColor Green
    $powContent = [System.IO.File]::ReadAllText($presetPow)
    $diaContent = [System.IO.File]::ReadAllText($presetDia)
} else {
    Write-Host "  No exact preset on disk - generating from rules." -ForegroundColor Yellow
    $powContent = New-PoworkerContent $cpuEntry $gpuEntry $comboLabel
    $diaContent = New-DiaworkerContent $cpuEntry $gpuEntry $comboLabel
}

$paths = Install-Configs $powContent $diaContent
Write-Host "  Installed:" -ForegroundColor Green
foreach ($p in $paths) { Write-Host "    $p" }

Write-Host ""
Write-Host "  Next:"
Write-Host "    1. LIST-OPENCL-DEVICES.bat"
Write-Host "    2. Edit platform_id / device_ids if needed"
Write-Host "    3. START-AMD-HAC-MINING.bat"
Write-Host ""

if (-not $NoPause) { Read-Host "  Press Enter to close" }
exit 0