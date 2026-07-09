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
for %%E in (poworker.exe diaworker.exe list_opencl.exe miner-panel.exe) do (
    if not exist "%BIN%%%E" (
        echo  [MISSING] %%E
        set "MISSING=1"
    )
)
if "!MISSING!"=="1" (
    echo  Download: hacash-miner-only-windows-x64.zip
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
    copy /Y "%BIN%poworker.config.ini" "%BIN%diaworker.config.ini" >nul
    echo  [CREATED] diaworker.config.ini
)

powershell -NoProfile -Command ^
  "$dir='%BIN:\=\\%';" ^
  "foreach($f in @('poworker.config.ini','diaworker.config.ini')){" ^
  "  $p=Join-Path $dir $f; if(Test-Path $p){" ^
  "    $t=Get-Content $p -Raw;" ^
  "    $t=$t -replace '(?m)^opencl_dir\s*=.*','opencl_dir = x16rs/opencl/';" ^
  "    $t=$t -replace '(?m)^connect\s*=.*','connect = 127.0.0.1:8080';" ^
  "    Set-Content -Path $p -Value $t -NoNewline}}"

set "VCRUNTIME_OK=0"
if exist "%SystemRoot%\System32\vcruntime140.dll" set "VCRUNTIME_OK=1"
if "!VCRUNTIME_OK!"=="0" (
    set /p "VCREDIST=  Install VC++ Redistributable x64? [Y/N]: "
    if /i "!VCREDIST!"=="Y" call :install_vcredist
) else (
    echo  [OK] Visual C++ Runtime detected.
)
echo.

echo  Checking OpenCL...
echo  ----------------------------------------
"%BIN%list_opencl.exe"
echo  ----------------------------------------
echo.

echo  Setup complete. Ensure fullnode RPC is at 127.0.0.1:8080
echo  Then open miner-panel.exe and press Start.
echo.
set /p "LAUNCH=  Open miner-panel now? [Y/N]: "
if /i "!LAUNCH!"=="Y" start "" "%BIN%miner-panel.exe"
pause
exit /b 0

:write_default_poworker_ini
(
    echo connect = 127.0.0.1:8080
    echo supervene = 6
    echo.
    echo [efficiency]
    echo mode = profit
    echo stats_file = miner-stats.json
    echo.
    echo [gpu]
    echo use_opencl = true
    echo gpu_profile = amd_profit
    echo platform_id = 0
    echo device_ids = 0
    echo opencl_dir = x16rs/opencl/
    echo work_groups = 1536
    echo local_size = 256
    echo unit_size = 96
) > "%BIN%poworker.config.ini"
exit /b 0

:install_vcredist
set "VCR_TMP=%TEMP%\vc_redist.x64.exe"
powershell -NoProfile -Command "Invoke-WebRequest -Uri 'https://aka.ms/vs/17/release/vc_redist.x64.exe' -OutFile '%VCR_TMP%' -UseBasicParsing"
start /wait "" "%VCR_TMP%" /install /quiet /norestart
del "%VCR_TMP%" >nul 2>&1
exit /b 0