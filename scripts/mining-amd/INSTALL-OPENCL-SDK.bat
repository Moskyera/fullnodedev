@echo off
setlocal EnableDelayedExpansion
title Install OpenCL.lib (Khronos ICD via vcpkg)

set "REPO_ROOT=%~dp0..\.."
set "VCPKG_ROOT=%REPO_ROOT%\.tools\vcpkg"
set "OPENCL_LIB=%VCPKG_ROOT%\installed\x64-windows\lib\OpenCL.lib"
set "CARGO_CONFIG=%REPO_ROOT%\.cargo\config.toml"

echo.
echo  Installing OpenCL ICD loader (OpenCL.lib) for MSVC build...
echo  Repo: %REPO_ROOT%
echo.

if exist "%OPENCL_LIB%" (
    echo  Already installed: %OPENCL_LIB%
    goto :write_config
)

where git >nul 2>&1
if errorlevel 1 (
    echo  ERROR: git not found. Install Git for Windows first.
    goto :fail
)

if not exist "%VCPKG_ROOT%" (
    echo  Cloning vcpkg into .tools\vcpkg ...
    git clone --depth 1 https://github.com/microsoft/vcpkg.git "%VCPKG_ROOT%"
    if errorlevel 1 goto :fail
)

if not exist "%VCPKG_ROOT%\vcpkg.exe" (
    echo  Bootstrapping vcpkg...
    call "%VCPKG_ROOT%\bootstrap-vcpkg.bat" -disableMetrics
    if errorlevel 1 goto :fail
)

echo  Installing opencl:x64-windows (may take a few minutes)...
"%VCPKG_ROOT%\vcpkg.exe" install opencl:x64-windows
if errorlevel 1 goto :fail

if not exist "%OPENCL_LIB%" (
    echo  ERROR: vcpkg finished but OpenCL.lib not found at:
    echo    %OPENCL_LIB%
    goto :fail
)

echo  OK: %OPENCL_LIB%

:write_config
if not exist "%REPO_ROOT%\.cargo" mkdir "%REPO_ROOT%\.cargo"

powershell -NoProfile -Command ^
  "$dir=(Resolve-Path (Split-Path '%OPENCL_LIB%')).Path;" ^
  "$esc=$dir -replace '\\','\\\\';" ^
  "$lines=@('[target.x86_64-pc-windows-msvc]','rustflags = [\"-Clink-arg=/LIBPATH:{0}\"]' -f $esc);" ^
  "$utf8=New-Object System.Text.UTF8Encoding $false;" ^
  "[System.IO.File]::WriteAllText('%CARGO_CONFIG%', ($lines -join [Environment]::NewLine) + [Environment]::NewLine, $utf8)"

echo  Wrote %CARGO_CONFIG%
echo.
echo  Now run: scripts\mining-amd\BUILD-AMD-MINER.bat
echo.
exit /b 0

:fail
echo.
echo  Manual alternative:
echo    1. Install Khronos OpenCL-SDK to:
echo       C:\Program Files (x86)\OCL_SDK_Light\lib\x86_64\OpenCL.lib
echo    2. Or run FIND-OPENCL-LIB.bat if .lib exists elsewhere
echo.
exit /b 1