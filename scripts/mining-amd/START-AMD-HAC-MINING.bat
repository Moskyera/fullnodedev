@echo off
setlocal
title HAC miner (AMD OpenCL + Ryzen CPU)

set "REPO_ROOT=%~dp0..\.."
set "RUN_DIR=%REPO_ROOT%\target\release"
if not exist "%RUN_DIR%\poworker.exe" set "RUN_DIR=%REPO_ROOT%\target\debug"

if not exist "%RUN_DIR%\poworker.exe" (
    echo poworker.exe not found. Run BUILD-AMD-MINER.bat first.
    pause
    exit /b 1
)

if not exist "%RUN_DIR%\poworker.config.ini" (
    echo Missing poworker.config.ini in %RUN_DIR%
    echo Run INSTALL-CONFIGS.bat first.
    pause
    exit /b 1
)

echo Starting HAC block miner from %RUN_DIR%
echo Requires fullnode RPC on connect= in poworker.config.ini
echo.
cd /d "%RUN_DIR%"
poworker.exe
pause
exit /b 0