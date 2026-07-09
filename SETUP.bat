@echo off
setlocal EnableDelayedExpansion
title HAC Miner - First-Time Setup
cd /d "%~dp0"

echo.
echo  ============================================
echo   HAC Miner Setup  (Windows)
echo   By Mosky
echo  ============================================
echo.

:: --- Locate miner folder (release zip OR dev build) ---
set "BIN=%~dp0"
if not exist "%BIN%miner-panel.exe" (
    if exist "%~dp0target\release\miner-panel.exe" (
        set "BIN=%~dp0target\release\"
    ) else if exist "%~dp0target\debug\miner-panel.exe" (
        set "BIN=%~dp0target\debug\"
    )
)
cd /d "%BIN%"
echo  Working folder: %BIN%
echo.

set "SCRIPT_AMD=%~dp0scripts\mining-amd"
if not exist "%SCRIPT_AMD%" set "SCRIPT_AMD=%~dp0"

:: --- 1. Check required executables ---
set "MISSING=0"
for %%E in (hacash.exe poworker.exe diaworker.exe list_opencl.exe miner-panel.exe) do (
    if not exist "%BIN%%%E" (
        echo  [MISSING] %%E
        set "MISSING=1"
    )
)
if "!MISSING!"=="1" (
    echo.
    echo  ERROR: Incomplete package.
    echo  Download hacash-miner-FULL-windows-x64.zip from GitHub Releases
    echo  or build from source:
    echo    scripts\mining-amd\BUILD-AMD-MINER.bat
    echo    scripts\mining-amd\BUILD-MINER-PANEL.bat
    echo.
    pause
    exit /b 1
)
echo  [OK] All miner executables found.
echo.

:: --- 2. Check OpenCL kernel files ---
if not exist "%BIN%x16rs\opencl\x16rs_main.cl" (
    echo  [ERROR] Missing folder: x16rs\opencl\
    echo          GPU mining will not work without the .cl kernel files.
    echo.
    pause
    exit /b 1
)
echo  [OK] OpenCL kernels found.
echo.

:: --- 3. Worker configs ---
if not exist "%BIN%poworker.config.ini" (
    if exist "%SCRIPT_AMD%poworker.amd.ini.example" (
        copy /Y "%SCRIPT_AMD%poworker.amd.ini.example" "%BIN%poworker.config.ini" >nul
    ) else (
        call :write_default_poworker_ini
    )
    echo  [CREATED] poworker.config.ini
) else (
    echo  [OK] poworker.config.ini exists.
)

if not exist "%BIN%diaworker.config.ini" (
    if exist "%SCRIPT_AMD%diaworker.amd.ini.example" (
        copy /Y "%SCRIPT_AMD%diaworker.amd.ini.example" "%BIN%diaworker.config.ini" >nul
    ) else (
        copy /Y "%BIN%poworker.config.ini" "%BIN%diaworker.config.ini" >nul 2>nul
    )
    echo  [CREATED] diaworker.config.ini
) else (
    echo  [OK] diaworker.config.ini exists.
)

:: Fix opencl_dir for flat release layout
powershell -NoProfile -Command ^
  "$dir='%BIN:\=\\%';" ^
  "foreach($f in @('poworker.config.ini','diaworker.config.ini')){" ^
  "  $p=Join-Path $dir $f; if(Test-Path $p){" ^
  "    $t=Get-Content $p -Raw;" ^
  "    $t=$t -replace '(?m)^opencl_dir\s*=.*','opencl_dir = x16rs/opencl/';" ^
  "    Set-Content -Path $p -Value $t -NoNewline}}"

:: --- 4. Fullnode config template ---
if not exist "%BIN%hacash.config.ini" (
    if exist "%~dp0hacash.config.ini" (
        copy /Y "%~dp0hacash.config.ini" "%BIN%hacash.config.ini" >nul
        echo  [CREATED] hacash.config.ini  (from template)
    ) else (
        call :write_default_hacash_ini
        echo  [CREATED] hacash.config.ini  (edit your wallet!)
    )
) else (
    echo  [OK] hacash.config.ini exists.
)
echo.

:: --- 5. Visual C++ Redistributable (required for MSVC-built .exe) ---
set "VCRUNTIME_OK=0"
where vcruntime140.dll >nul 2>&1 && set "VCRUNTIME_OK=1"
if exist "%SystemRoot%\System32\vcruntime140.dll" set "VCRUNTIME_OK=1"

if "!VCRUNTIME_OK!"=="0" (
    echo  [WARN] Visual C++ Runtime may be missing.
    set /p "VCREDIST=  Install VC++ Redistributable 2015-2022 x64 now? [Y/N]: "
    if /i "!VCREDIST!"=="Y" call :install_vcredist
) else (
    echo  [OK] Visual C++ Runtime detected.
)
echo.

:: --- 6. OpenCL / GPU driver check ---
echo  Checking OpenCL (GPU drivers)...
echo  ----------------------------------------
"%BIN%list_opencl.exe"
set "OCL_ERR=!ERRORLEVEL!"
echo  ----------------------------------------
if not "!OCL_ERR!"=="0" (
    echo.
    echo  [WARN] OpenCL not available or no GPU detected.
    echo.
    echo  For GPU mining install:
    echo    AMD  - Adrenalin drivers  https://www.amd.com/en/support
    echo    NVIDIA - Game Ready driver https://www.nvidia.com/drivers
    echo.
    echo  CPU-only fallback: set use_opencl = false in poworker.config.ini
    echo.
    set /p "OPEN_DRV=  Open GPU driver download page in browser? [Y/N]: "
    if /i "!OPEN_DRV!"=="Y" start https://www.amd.com/en/support/download/drivers.html
) else (
    echo.
    echo  [OK] OpenCL is working. Note platform_id and device_ids above.
    echo       Set them in miner-panel Settings or in poworker.config.ini
)
echo.

:: --- 7. Copy logo if present ---
if exist "%~dp0miner-panel\assets\hhh.png" (
    if not exist "%BIN%hhh.png" copy /Y "%~dp0miner-panel\assets\hhh.png" "%BIN%hhh.png" >nul
)

:: --- Done ---
echo  ============================================
echo   Setup complete!
echo  ============================================
echo.
echo   Next steps:
echo     1. Open miner-panel.exe
echo     2. Settings - pick CPU/GPU, enter wallet, Save
echo     3. Start mining (panel can auto-start fullnode)
echo.
echo   Solo mining needs hacash.exe running with RPC on port 8080.
echo   Edit hacash.config.ini - set [miner] reward wallet before first run.
echo.

set /p "LAUNCH=  Open HAC Miner Panel now? [Y/N]: "
if /i "!LAUNCH!"=="Y" (
    start "" "%BIN%miner-panel.exe"
)

pause
exit /b 0

:: ---------- helpers ----------

:write_default_poworker_ini
(
    echo connect = 127.0.0.1:8080
    echo supervene = 6
    echo.
    echo [efficiency]
    echo mode = profit
    echo power_cost_kwh = 0.15
    echo stats_file = miner-stats.json
    echo.
    echo [gpu]
    echo use_opencl = true
    echo cpu_assist = true
    echo gpu_profile = amd_profit
    echo platform_id = 0
    echo device_ids = 0
    echo opencl_dir = x16rs/opencl/
    echo work_groups = 1536
    echo local_size = 256
    echo unit_size = 96
) > "%BIN%poworker.config.ini"
exit /b 0

:write_default_hacash_ini
(
    echo [server]
    echo enable = true
    echo listen = 8080
    echo diamond_form = true
    echo.
    echo [miner]
    echo enable = true
    echo reward = YOUR_HAC_WALLET_ADDRESS
    echo.
    echo [diamondminer]
    echo enable = false
    echo reward = YOUR_HACD_PRIVAKEY_3x
) > "%BIN%hacash.config.ini"
exit /b 0

:install_vcredist
set "VCR_TMP=%TEMP%\vc_redist.x64.exe"
echo  Downloading VC++ Redistributable...
powershell -NoProfile -Command ^
  "try { Invoke-WebRequest -Uri 'https://aka.ms/vs/17/release/vc_redist.x64.exe' -OutFile '%VCR_TMP%' -UseBasicParsing; exit 0 } catch { exit 1 }"
if errorlevel 1 (
    echo  [ERROR] Download failed. Install manually:
    echo    https://aka.ms/vs/17/release/vc_redist.x64.exe
    exit /b 0
)
echo  Installing (may need Administrator)...
start /wait "" "%VCR_TMP%" /install /quiet /norestart
if errorlevel 1 (
    echo  [WARN] Silent install failed. Running interactive installer...
    start /wait "" "%VCR_TMP%"
)
del "%VCR_TMP%" >nul 2>&1
echo  [OK] VC++ Redistributable install finished.
exit /b 0