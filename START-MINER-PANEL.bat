@echo off
setlocal EnableDelayedExpansion
title HAC OpenCL Miner
cd /d "%~dp0"

set "MISSING=0"
for %%E in (miner-panel.exe poworker.exe diaworker.exe diagnose_opencl.exe) do (
    if not exist "%%E" (
        echo [MISSING] %%E
        set "MISSING=1"
    )
)
if not exist "x16rs\opencl\x16rs_main.cl" (
    echo [MISSING] x16rs\opencl\x16rs_main.cl
    set "MISSING=1"
)
if "!MISSING!"=="1" (
    echo.
    echo This miner package is incomplete. Extract the complete release ZIP
    echo into one folder, then run START-MINER-PANEL.bat again.
    pause
    exit /b 1
)

start "" "%~dp0miner-panel.exe"
exit /b 0