@echo off
setlocal
title Build HAC Miner Panel (GUI)

set "REPO_ROOT=%~dp0..\.."
cd /d "%REPO_ROOT%"

call "%~dp0FIND-OPENCL-LIB.bat" >nul 2>&1

echo.
echo  Building miner-panel.exe (GUI setup + dashboard)...
echo.

cargo build --release -p miner-panel
if errorlevel 1 (
    echo BUILD FAILED
    pause
    exit /b 1
)

set "OUT=%REPO_ROOT%\target\release"
copy /Y "%OUT%\miner-panel.exe" "%OUT%\" >nul 2>&1

echo.
echo  OK: %OUT%\miner-panel.exe
echo.
echo  Copy to the same folder as poworker.exe and hacash.exe, then double-click.
echo  Or run from: %OUT%
echo.
pause
exit /b 0