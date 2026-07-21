@echo off
cd /d C:\Users\KQHEX\Documents\hacash-fullnodedev
echo === SAFE START %DATE% %TIME% ===> safe-cycle.log
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_build-and-mine-safe.ps1 >> safe-cycle.log 2>&1
echo === SAFE BUILD EXIT %ERRORLEVEL% %DATE% %TIME% ===>> safe-cycle.log
timeout /t 90 /nobreak >nul
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\_collect-after-mine.ps1 >> safe-cycle.log 2>&1
echo === SAFE DONE %DATE% %TIME% ===>> safe-cycle.log