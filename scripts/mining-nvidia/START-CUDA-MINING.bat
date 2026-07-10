@echo off
setlocal
title HAC miner (NVIDIA CUDA)

set "REPO_ROOT=%~dp0..\.."
set "RUN_DIR=%REPO_ROOT%\target\release"
if not exist "%RUN_DIR%\poworker.exe" set "RUN_DIR=%REPO_ROOT%\target\debug"

if not exist "%RUN_DIR%\poworker.exe" (
    echo poworker.exe not found. Run BUILD-CUDA-MINER.bat first.
    pause
    exit /b 1
)

if not exist "%RUN_DIR%\poworker.config.ini" (
    echo Missing poworker.config.ini — running INSTALL-CUDA-CONFIG.bat ...
    call "%~dp0INSTALL-CUDA-CONFIG.bat"
    if errorlevel 1 pause & exit /b 1
)

echo Starting CUDA block miner from %RUN_DIR%
echo Requires fullnode RPC on connect= in poworker.config.ini
echo Expected: [CUDA] Device #0: NVIDIA GeForce RTX ...
echo.
cd /d "%RUN_DIR%"
poworker.exe
pause
exit /b 0