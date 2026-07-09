@echo off
setlocal
title HAC Miner Panel (GUI)

set "REPO_ROOT=%~dp0..\.."
set "BIN=%REPO_ROOT%\target\release"

if not exist "%BIN%\miner-panel.exe" (
    echo miner-panel.exe not found. Run BUILD-MINER-PANEL.bat first.
    pause
    exit /b 1
)

cd /d "%BIN%"
start "" "%BIN%\miner-panel.exe"
exit /b 0