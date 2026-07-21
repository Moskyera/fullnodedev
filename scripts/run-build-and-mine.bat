@echo off
cd /d C:\Users\KQHEX\Documents\hacash-fullnodedev
echo === BAT START %DATE% %TIME% ===> build-direct.log
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\build-and-mine.ps1 >> build-direct.log 2>&1
echo === BAT EXIT %ERRORLEVEL% %DATE% %TIME% ===>> build-direct.log
timeout /t 90 /nobreak >nul
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_collect-after-mine.ps1 >> build-direct.log 2>&1
echo === BAT DONE %DATE% %TIME% ===>> build-direct.log