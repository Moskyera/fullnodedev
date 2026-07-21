@echo off
setlocal EnableDelayedExpansion
title HAC Miner - Workers Setup (no fullnode)
cd /d "%~dp0"

echo.
echo  ============================================
echo   HAC Miner - WORKERS ONLY
echo   By Mosky
echo  ============================================
echo.
echo  Use this package if you ALREADY have:
echo    - hacash.exe fullnode running with RPC on port 8080
echo    - hacash.config.ini with your wallet
echo.
echo  Clean PC? Use the FULL package instead.
echo.

set "BIN=%~dp0"
cd /d "%BIN%"

set "MISSING=0"
for %%E in (poworker.exe diaworker.exe list_opencl.exe diagnose_opencl.exe miner-panel.exe) do (
    if not exist "%BIN%%%E" (
        echo  [MISSING] %%E
        set "MISSING=1"
    )
)
if "!MISSING!"=="1" (
    echo  Download: hacash-miner-only-windows-x64*.zip
    pause
    exit /b 1
)
echo  [OK] Miner executables found.
echo.

if not exist "%BIN%x16rs\opencl\x16rs_main.cl" (
    echo  [ERROR] Missing x16rs\opencl\ kernels.
    pause
    exit /b 1
)
echo  [OK] OpenCL kernels found.
echo.

if not exist "%BIN%poworker.config.ini" (
    call :write_default_poworker_ini
    echo  [CREATED] poworker.config.ini
)
if not exist "%BIN%diaworker.config.ini" (
    call :write_default_diaworker_ini
    echo  [CREATED] diaworker.config.ini - HACD CPU-only
)

powershell -NoProfile -Command ^
  "$dir='%BIN:\=\\%';" ^
  "foreach($f in @('poworker.config.ini','diaworker.config.ini')){" ^
  "  $p=Join-Path $dir $f; if(Test-Path $p){" ^
  "    $t=Get-Content $p -Raw;" ^
  "    $t=$t -replace '(?m)^opencl_dir\s*=.*','opencl_dir = x16rs/opencl/';" ^
  "    if($t -notmatch '(?m)^connect\s*=\s*\S'){" ^
  "      if($t -match '(?m)^connect\s*='){ $t=$t -replace '(?m)^connect\s*=.*','connect = 127.0.0.1:8080' }" ^
  "      else { $t = ('connect = 127.0.0.1:8080' + [Environment]::NewLine + $t) } }" ^
  "    Set-Content -Path $p -Value $t -NoNewline}}"

set "VCRUNTIME_OK=0"
if exist "%SystemRoot%\System32\vcruntime140.dll" set "VCRUNTIME_OK=1"
if "!VCRUNTIME_OK!"=="0" (
    set /p "VCREDIST=  Install VC++ Redistributable x64? [Y/N]: "
    if /i "!VCREDIST!"=="Y" (
        call :install_vcredist
        if errorlevel 1 (
            echo  [ERROR] Setup cannot continue without the required runtime.
            pause
            exit /b 1
        )
    )
) else (
    echo  [OK] Visual C++ Runtime detected.
)
echo.

echo  Checking OpenCL...
echo  ----------------------------------------
"%BIN%list_opencl.exe"
set "OCL_ERR=!ERRORLEVEL!"
echo  ----------------------------------------
if not "!OCL_ERR!"=="0" (
    echo.
    echo  [WARN] No usable OpenCL GPU was detected.
    echo         Install your GPU vendor's OpenCL driver and run setup again.
    echo         HACD remains available because it is CPU-only.
) else (
    echo.
    echo  [OK] OpenCL is working.
)
echo.

echo  Setup complete. Ensure fullnode RPC is at 127.0.0.1:8080
echo  Then open miner-panel.exe, enter your wallet, and press Start.
echo  The OpenCL GPU is detected automatically.
echo.
set /p "LAUNCH=  Open miner-panel now? [Y/N]: "
if /i "!LAUNCH!"=="Y" start "" "%BIN%miner-panel.exe"
pause
exit /b 0

:write_default_poworker_ini
(
    echo connect = 127.0.0.1:8080
    echo supervene = 0
    echo.
    echo [efficiency]
    echo mode = profit
    echo stats_file = miner-stats.json
    echo max_temp_c = 0
    echo throttle_work_groups = 32
    echo benchmark_seconds = 0
    echo.
    echo [gpu]
    echo use_opencl = true
    echo use_cuda = false
    echo cpu_assist = false
    echo gpu_profile = amd_balanced
    echo platform_id = 0
    echo device_ids = 0
    echo opencl_dir = x16rs/opencl/
    echo work_groups = 64
    echo local_size = 256
    echo unit_size = 64
) > "%BIN%poworker.config.ini"
exit /b 0

:write_default_diaworker_ini
(
    echo connect = 127.0.0.1:8080
    echo supervene = 4
    echo.
    echo [efficiency]
    echo mode = profit
    echo cpu_watts_per_thread = 8
    echo dynamic_supervene = true
    echo supervene_min = 1
    echo supervene_max = 0
    echo benchmark_seconds = 0
    echo.
    echo [gpu]
    echo use_opencl = false
    echo use_cuda = false
    echo cpu_assist = false
) > "%BIN%diaworker.config.ini"
exit /b 0

:install_vcredist
set "VCR_TMP=%TEMP%\vc_redist.x64.exe"
echo  Downloading VC++ Redistributable...
powershell -NoProfile -Command ^
  "try { Invoke-WebRequest -Uri 'https://aka.ms/vs/17/release/vc_redist.x64.exe' -OutFile '%VCR_TMP%' -UseBasicParsing; exit 0 } catch { exit 1 }"
if errorlevel 1 (
    echo  [ERROR] Download failed. Install manually:
    echo    https://aka.ms/vs/17/release/vc_redist.x64.exe
    exit /b 1
)
echo  Installing (may need Administrator)...
start /wait "" "%VCR_TMP%" /install /quiet /norestart
if errorlevel 1 (
    echo  [WARN] Silent install failed. Running interactive installer...
    start /wait "" "%VCR_TMP%"
    if errorlevel 1 (
        echo  [ERROR] VC++ Redistributable installation failed.
        del "%VCR_TMP%" >nul 2>&1
        exit /b 1
    )
)
del "%VCR_TMP%" >nul 2>&1
echo  [OK] VC++ Redistributable install finished.
exit /b 0