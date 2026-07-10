@echo off
setlocal
cd /d "%~dp0..\.."

if "%CUDA_PATH%"=="" (
  for %%V in (v13.3 v13.0 v12.8 v12.6 v12.5 v12.4) do (
    if exist "C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\%%V\bin\nvcc.exe" (
      set "CUDA_PATH=C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\%%V"
      goto :found
    )
  )
)
:found

if exist "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat" (
  call "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
) else if exist "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" (
  call "C:\Program Files\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
)

echo Building Hacash CUDA miner (poworker + x16rs-cuda)...
cargo build --release --bin poworker --features cuda
if errorlevel 1 exit /b 1

echo.
echo OK: target\release\poworker.exe
echo.
echo Next (RTX machine with GPU):
echo   TEST-CUDA-GPU.bat
echo   INSTALL-CUDA-CONFIG.bat
echo   START-CUDA-MINING.bat
echo See HANDOFF-RTX.md for full checklist.
endlocal