@echo off
setlocal EnableDelayedExpansion
title Tune AMD miner efficiency

set "REPO_ROOT=%~dp0..\.."
set "SCRIPT_DIR=%~dp0"
set /a CORES=%NUMBER_OF_PROCESSORS%
set /a CPU_THREADS=%CORES%
if %CPU_THREADS% gtr 8 set /a CPU_THREADS=8
if %CPU_THREADS% lss 2 set /a CPU_THREADS=2

echo.
echo  Ryzen logical cores: %CORES%
echo  Suggested supervene (CPU assist): %CPU_THREADS%
echo  GPU profile: amd_performance (edit in config to amd_max if VRAM allows)
echo.

for %%D in (debug release) do (
    set "CFG=%REPO_ROOT%\target\%%D\poworker.config.ini"
    if exist "!CFG!" (
        echo Updating !CFG!
        powershell -NoProfile -Command ^
          "$p='!CFG!'; $t=Get-Content $p -Raw; $t=$t -replace '(?m)^supervene\s*=\s*\d+','supervene = %CPU_THREADS%'; if($t -notmatch 'cpu_assist'){$t=$t -replace '(\[gpu\])','$1`ncpu_assist = true'}; if($t -notmatch 'gpu_profile'){$t=$t -replace '(\[gpu\])','$1`ngpu_profile = amd_performance'}; Set-Content $p $t"
    )
    set "CFG=%REPO_ROOT%\target\%%D\diaworker.config.ini"
    if exist "!CFG!" (
        powershell -NoProfile -Command ^
          "$p='!CFG!'; $t=Get-Content $p -Raw; $t=$t -replace '(?m)^supervene\s*=\s*\d+','supervene = %CPU_THREADS%'; Set-Content $p $t"
    )
)

echo.
echo  Done. Rebuild if code changed: BUILD-AMD-MINER.bat
echo  Delete old *.bin in x16rs/opencl if kernels were updated.
echo.
pause
exit /b 0