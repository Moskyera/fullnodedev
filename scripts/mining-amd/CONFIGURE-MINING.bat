@echo off
setlocal
title Configure AMD mining (type your CPU + GPU)

cd /d "%~dp0"
chcp 65001 >nul
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0GENERATE-MINING-CONFIG.ps1"
exit /b %ERRORLEVEL%