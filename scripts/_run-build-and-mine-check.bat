@echo off
cd /d C:\Users\KQHEX\Documents\hacash-fullnodedev
powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\_run-build-and-mine-check.ps1 > C:\Users\KQHEX\Documents\hacash-fullnodedev\bm-check-out.txt 2>&1
exit /b %ERRORLEVEL%