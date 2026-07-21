@echo off
setlocal
title HACD diamond miner (CPU only)

set "REPO_ROOT=%~dp0..\.."
set "RUN_DIR=%REPO_ROOT%\target\release"
if not exist "%RUN_DIR%\diaworker.exe" set "RUN_DIR=%REPO_ROOT%\target\debug"

if not exist "%RUN_DIR%\diaworker.exe" (
    echo diaworker.exe not found. Run BUILD-AMD-MINER.bat first.
    pause
    exit /b 1
)

if not exist "%RUN_DIR%\diaworker.config.ini" (
    echo Missing diaworker.config.ini in %RUN_DIR%
    echo Run INSTALL-CONFIGS.bat first.
    pause
    exit /b 1
)

echo Starting HACD CPU diamond miner from %RUN_DIR%
echo HACD does not use OpenCL; supervene controls CPU threads.
echo Requires fullnode with [diamondminer] enable = true
echo.
cd /d "%RUN_DIR%"
diaworker.exe
pause
exit /b 0