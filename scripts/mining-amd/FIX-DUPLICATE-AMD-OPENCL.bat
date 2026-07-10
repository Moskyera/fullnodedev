@echo off
setlocal
title Remove duplicate AMD OpenCL platform (3652)

echo.
echo  === Fix duplicate AMD OpenCL (gfx1201 / RX 9070 XT) ===
echo.
echo  Your scan shows TWO AMD platforms (3679 + 3652). The miner caps work_groups
echo  until only ONE platform remains. iGPU disable in Device Manager is not enough
echo  if the old AMD-APP 3652 ICD is still registered.
echo.
echo  --- Step 1: Verify ---
cd /d "%~dp0..\..\target\release"
if exist diagnose_opencl.exe (
    diagnose_opencl.exe --report diagnose-opencl.json
) else (
    echo  Build first: cargo build --release --features ocl
)
echo.
echo  --- Step 2: Clean driver (recommended) ---
echo   1. Download DDU from https://www.guru3d.com/download/display-driver-uninstaller-download/
echo   2. Boot safe mode, run DDU, remove AMD GPU driver
echo   3. Reboot, install latest Adrenalin ONLY (no old AMD APP SDK)
echo   4. Device Manager: keep iGPU DISABLED, only RX 9070 XT enabled
echo.
echo  --- Step 3: Optional registry (admin) ---
echo   Remove stale OpenCL platform keys under:
echo   HKLM\SOFTWARE\Khronos\OpenCL\Vendors
echo   (Keep only the newest amdocl64.dll path)
echo.
echo  --- Step 4: Auto-fix (run as Admin) ---
echo   scripts\mining-amd\DISABLE-AMD-OPENCL-3652.bat
echo.
echo  --- Step 5: Re-test ---
echo   scripts\mining-amd\DIAGNOSE-AMD-GPU.bat
echo   Expect ONE AMD platform and gpu_hashrate_hps ^> 0 in miner-stats.json
echo.
pause