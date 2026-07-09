@echo off
setlocal
title Pack Windows miner release ZIP

set "REPO_ROOT=%~dp0..\.."
cd /d "%REPO_ROOT%"

if not exist "target\release\miner-panel.exe" (
    echo Build first:
    echo   BUILD-AMD-MINER.bat
    echo   BUILD-MINER-PANEL.bat
    pause
    exit /b 1
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%REPO_ROOT%\scripts\pack-release.ps1" -Version dev
pause
exit /b 0