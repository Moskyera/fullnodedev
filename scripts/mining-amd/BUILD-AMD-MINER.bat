@echo off
setlocal EnableDelayedExpansion
title Build Hacash AMD miners (OpenCL)

set "REPO_ROOT=%~dp0..\.."
cd /d "%REPO_ROOT%"

echo.
echo  Building poworker + diaworker + list_opencl with OpenCL (AMD/NVIDIA)...
echo  Repo: %REPO_ROOT%
echo.

call "%~dp0FIND-OPENCL-LIB.bat"
if errorlevel 1 (
    echo.
    echo  OpenCL.lib not found. Try automatic install? [Y/N]
    set /p DO_INSTALL="  "
    if /i "!DO_INSTALL!"=="Y" (
        call "%~dp0INSTALL-OPENCL-SDK.bat"
        if errorlevel 1 (
            pause
            exit /b 1
        )
    ) else (
        echo  BUILD ABORTED — run INSTALL-OPENCL-SDK.bat or install AMD drivers.
        pause
        exit /b 1
    )
)

cargo build --release --features ocl
if errorlevel 1 (
    echo.
    echo  BUILD FAILED
    echo  If you see LNK1181 / OpenCL.lib: run FIND-OPENCL-LIB.bat
    pause
    exit /b 1
)

echo.
echo  OK: %REPO_ROOT%\target\release\poworker.exe
echo      %REPO_ROOT%\target\release\diaworker.exe
echo      %REPO_ROOT%\target\release\list_opencl.exe
echo.
echo  Next: CONFIGURE-MINING.bat or INSTALL-CONFIGS.bat
echo        LIST-OPENCL-DEVICES.bat
echo.
pause
exit /b 0