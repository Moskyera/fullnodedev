@echo off
setlocal
title Build Hacash AMD miners (OpenCL)

set "REPO_ROOT=%~dp0..\.."
cd /d "%REPO_ROOT%"

echo.
echo  Building poworker + diaworker + list_opencl with OpenCL (AMD/NVIDIA)...
echo  Repo: %REPO_ROOT%
echo.

cargo build --release --features ocl
if errorlevel 1 (
    echo.
    echo  BUILD FAILED
    pause
    exit /b 1
)

echo.
echo  OK: %REPO_ROOT%\target\release\poworker.exe
echo      %REPO_ROOT%\target\release\diaworker.exe
echo      %REPO_ROOT%\target\release\list_opencl.exe
echo.
echo  Next: INSTALL-CONFIGS.bat then LIST-OPENCL-DEVICES.bat
echo.
pause
exit /b 0