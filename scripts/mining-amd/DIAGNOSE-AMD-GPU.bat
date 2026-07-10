@echo off
setlocal EnableDelayedExpansion
title AMD OpenCL diagnostic (gfx1201 / RX 9070 XT)

set "REPO_ROOT=%~dp0..\.."
set "BIN=%REPO_ROOT%\target\release"
if not exist "%BIN%\poworker.exe" set "BIN=%REPO_ROOT%\target\debug"

echo.
echo  === Hacash AMD GPU diagnostic ===
echo  Repo: %REPO_ROOT%
echo  Bin:  %BIN%
echo.

if not exist "%BIN%\diagnose_opencl.exe" (
    echo  Building diagnose_opencl + poworker...
    cd /d "%REPO_ROOT%"
    cargo build --release --features ocl
    if errorlevel 1 (
        echo  BUILD FAILED
        pause
        exit /b 1
    )
)

echo --- 1) GPU drivers (Windows) ---
powershell -NoProfile -Command "Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion, DriverDate | Format-Table -AutoSize"

echo.
echo --- 2) OpenCL scan ---
cd /d "%BIN%"
"%BIN%\diagnose_opencl.exe" --report "%BIN%\diagnose-opencl.json"

echo.
echo --- 3) Kernel cache ---
dir /b "%REPO_ROOT%\x16rs\opencl\*.bin" 2>nul
if errorlevel 1 echo   (no cached .bin files)

echo.
set "CFG=%BIN%\poworker.config.ini"
if not exist "%CFG%" (
    echo  No %CFG% — skip benchmark. Run miner-panel Save first.
    goto :done
)

echo --- 4) Pure-GPU benchmark (45s, cpu_assist=false) ---
echo  Stopping running poworker if any...
taskkill /IM poworker.exe /F >nul 2>&1

copy /Y "%CFG%" "%CFG%.diagbak" >nul
powershell -NoProfile -Command ^
  "$p='%CFG%'; $t=Get-Content $p -Raw; $t=$t -replace '(?m)^cpu_assist\s*=\s*\w+','cpu_assist = false'; if($t -notmatch 'benchmark_seconds'){ $t += \"`n[efficiency]`nbenchmark_seconds = 45`n\" } else { $t = $t -replace '(?m)^benchmark_seconds\s*=\s*\d+','benchmark_seconds = 45' }; Set-Content $p $t -NoNewline"

echo  Running poworker benchmark — watch for SKIPPED/OOM lines...
"%BIN%\poworker.exe" > "%BIN%\diagnose-benchmark.log" 2>&1
type "%BIN%\diagnose-benchmark.log"

copy /Y "%CFG%.diagbak" "%CFG%" >nul
del "%CFG%.diagbak" >nul 2>&1

echo.
echo --- 5) Report files ---
echo   %BIN%\diagnose-opencl.json
echo   %BIN%\diagnose-benchmark.log
echo.
echo  FIX duplicate AMD platform (most common gfx1201 issue):
echo    1. Win+X -^> Device Manager -^> Display adapters
echo    2. Right-click "AMD Radeon(TM) Graphics" (iGPU, gfx1036) -^> Disable device
echo    3. Reboot, then re-run this script
echo  If CL_OUT_OF_RESOURCES: reboot PC, then retry with work_groups=1024.
echo  If benchmark GPU-only ^> 3 MH/s but live low: check gpu_hashrate in miner-stats.json.

:done
echo.
pause
exit /b 0