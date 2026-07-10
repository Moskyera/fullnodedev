@echo off
setlocal
cd /d "%~dp0\.."

REM Single-instance HACD smoke test (avoids overlapping fullnode races).
taskkill /F /IM poworker.exe /IM diaworker.exe /IM fullnode.exe >nul 2>&1
timeout /t 3 /nobreak >nul

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0test-hacd.ps1" %*
set EXITCODE=%ERRORLEVEL%
exit /b %EXITCODE%