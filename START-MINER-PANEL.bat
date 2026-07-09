@echo off
setlocal
title HAC Miner Panel
cd /d "%~dp0"
if not exist "miner-panel.exe" (
    echo miner-panel.exe not found in this folder.
    echo Run SETUP.bat first or extract the full release ZIP.
    pause
    exit /b 1
)
start "" "%~dp0miner-panel.exe"
exit /b 0