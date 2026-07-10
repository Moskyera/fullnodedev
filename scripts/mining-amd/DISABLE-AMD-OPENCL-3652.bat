@echo off
setlocal EnableDelayedExpansion
title Disable AMD OpenCL 3652 (keep 3679 only)

:: Self-elevate to Administrator
net session >nul 2>&1
if %errorlevel% neq 0 (
    echo Requesting Administrator privileges...
    powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    exit /b
)

echo.
echo  === Disable older AMD OpenCL ICD (3652) ===
echo.

set "GHOST=SWD\DRIVERENUM\AMDOCL&5&22f726a&0"
set "ICD3679=C:\Windows\System32\DriverStore\FileRepository\amdocl.inf_amd64_983c093b054f7226\amdocl64.dll"

echo --- 1) OpenCL scan BEFORE ---
cd /d "%~dp0..\..\target\release"
if exist diagnose_opencl.exe diagnose_opencl.exe --report diagnose-before.json

echo.
echo --- 2) Remove ghost AMD-OpenCL device (oem38 / iGPU stack) ---
pnputil /remove-device "%GHOST%"

echo.
echo --- 3) Pin OpenCL to AMD-APP 3679 ICD only (registry) ---
reg add "HKLM\SOFTWARE\Khronos\OpenCL\Vendors" /f >nul
reg delete "HKLM\SOFTWARE\Khronos\OpenCL\Vendors" /f >nul 2>&1
reg add "HKLM\SOFTWARE\Khronos\OpenCL\Vendors" /f >nul
reg add "HKLM\SOFTWARE\Khronos\OpenCL\Vendors" /v "%ICD3679%" /t REG_DWORD /d 0 /f

echo.
echo --- 4) Delete stale amdocl driver packages ---
for %%P in (oem38.inf oem4.inf oem88.inf oem101.inf oem110.inf oem113.inf) do (
    echo   Deleting %%P ...
    pnputil /delete-driver %%P /uninstall /force >nul 2>&1
)

echo.
echo --- 5) OpenCL scan AFTER ---
if exist diagnose_opencl.exe diagnose_opencl.exe --report diagnose-after.json

echo.
echo  EXPECT: ONE AMD platform (3679), gfx1201 only.
echo  If still two platforms: REBOOT, then run this script again.
echo.
pause
exit /b 0