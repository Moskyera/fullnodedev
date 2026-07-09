@echo off
setlocal EnableDelayedExpansion
title AMD mining autotune benchmark

set "REPO_ROOT=%~dp0..\.."

echo.
echo  Runs a short GPU benchmark and recommends gpu_profile.
echo  Sets benchmark_seconds=45 in target configs, then starts poworker.
echo.
echo  After benchmark completes, set benchmark_seconds=0 and gpu_profile as recommended.
echo.

for %%D in (debug release) do (
    set "CFG=%REPO_ROOT%\target\%%D\poworker.config.ini"
    if exist "!CFG!" (
        powershell -NoProfile -Command ^
          "$p='!CFG!'; $t=Get-Content $p -Raw; if($t -notmatch '\[efficiency\]'){ $t += \"`n[efficiency]`nmode = profit`nbenchmark_seconds = 45`n\" } else { $t = $t -replace '(?m)^benchmark_seconds\s*=\s*\d+','benchmark_seconds = 45' }; Set-Content $p $t"
        echo  Updated !CFG!
    )
)

echo.
echo  Starting poworker benchmark (45s total)...
echo.

if exist "%REPO_ROOT%\target\release\poworker.exe" (
    cd /d "%REPO_ROOT%\target\release"
    poworker.exe
) else if exist "%REPO_ROOT%\target\debug\poworker.exe" (
    cd /d "%REPO_ROOT%\target\debug"
    poworker.exe
) else (
    echo  Build first: scripts\mining-amd\BUILD-AMD-MINER.bat
)

echo.
pause
exit /b 0