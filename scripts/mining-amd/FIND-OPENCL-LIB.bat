@echo off
setlocal EnableDelayedExpansion
title Find OpenCL.lib for MSVC linker

echo.
echo  Searching for OpenCL.lib (required to link poworker/diaworker)...
echo.

set "FOUND="
set "FOUND_DIR="

for %%P in (
    "%ProgramFiles(x86)%\OCL_SDK_Light\lib\x86_64"
    "%ProgramFiles%\OCL_SDK_Light\lib\x86_64"
    "%AMDAPPSDKROOT%\lib\x86_64"
    "%CUDA_PATH%\lib\x64"
    "%INTELOCLSDKROOT%\lib\x64"
    "%ProgramFiles(x86)%\AMD APP SDK\2.9\lib\x86_64"
    "%ProgramFiles%\Khronos\OpenCL-SDK\lib"
) do (
    if exist "%%~P\OpenCL.lib" (
        set "FOUND=1"
        set "FOUND_DIR=%%~P"
        goto :found
    )
)

:: Recursive search in Program Files (slow, only if not found above)
for /f "delims=" %%F in ('where /r "%ProgramFiles%" OpenCL.lib 2^>nul') do (
    set "FOUND=1"
    for %%D in ("%%~dpF.") do set "FOUND_DIR=%%~fD"
    goto :found
)

:found
if defined FOUND (
    echo  Found: !FOUND_DIR!\OpenCL.lib
    set "LIB=!FOUND_DIR!;%LIB%"
    echo  Added to LIB for this session.
    echo.
    exit /b 0
)

echo  NOT FOUND: OpenCL.lib
echo.
echo  Install one of:
echo    1. AMD Adrenalin GPU drivers (recommended for RX cards)
echo    2. Khronos OpenCL SDK: https://github.com/KhronosGroup/OpenCL-SDK/releases
echo    3. CUDA Toolkit (includes OpenCL.lib) if you have NVIDIA tools
echo.
echo  After install, re-run this script or BUILD-AMD-MINER.bat
echo.
echo  Note: OpenCL.dll may exist in System32 but .lib is needed at BUILD time.
echo.
exit /b 1