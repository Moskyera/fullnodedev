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

echo === CUDA GPU validation (requires NVIDIA GPU) ===
echo.
echo [1/2] Genesis hash vector test...
cargo test -p x16rs-cuda --features cuda -- --nocapture
if errorlevel 1 (
  echo.
  echo FAILED: genesis GPU test
  exit /b 1
)

echo.
echo [2/2] Build poworker if needed...
if not exist "target\release\poworker.exe" (
  call "%~dp0BUILD-CUDA-MINER.bat"
  if errorlevel 1 exit /b 1
)

echo.
echo OK. Next:
echo   INSTALL-CUDA-CONFIG.bat
echo   START-CUDA-MINING.bat
echo.
echo Expected startup lines:
echo   [CUDA] Device #0: NVIDIA GeForce RTX ...
echo   [CUDA] Initialized device #0 work_groups=...
echo See HANDOFF-RTX.md for what to report back.
endlocal