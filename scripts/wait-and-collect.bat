@echo off
timeout /t 90 /nobreak >nul
powershell -NoProfile -ExecutionPolicy Bypass -File C:\Users\KQHEX\Documents\hacash-fullnodedev\scripts\_collect-after-mine.ps1 > C:\Users\KQHEX\Documents\hacash-fullnodedev\wait-collect.log 2>&1