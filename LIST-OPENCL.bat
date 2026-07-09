@echo off
setlocal
title OpenCL devices
cd /d "%~dp0"
if not exist "list_opencl.exe" (
    echo list_opencl.exe not found.
    pause
    exit /b 1
)
list_opencl.exe
echo.
pause
exit /b 0