@echo off
setlocal EnableDelayedExpansion
title Pick AMD mining preset (CPU + GPU)

set "REPO_ROOT=%~dp0..\.."
set "SCRIPT_DIR=%~dp0"
set "PRESETS=%SCRIPT_DIR%presets"

:menu
cls
echo.
echo  ============================================================
echo   HAC / HACD AMD Mining — CPU + GPU Presets
echo  ============================================================
echo.
echo   Type your hardware instead?  Run CONFIGURE-MINING.bat
echo   (e.g. CPU: 9950x   GPU: 7900xtx)
echo.
echo   Or pick a preset number below.
echo   Full list: scripts\mining-amd\PRESETS-INDEX.txt
echo.
echo  --- Ryzen 5 (entry) ---
echo   01  Ryzen 5  +  RX 6600 / 6600 XT     (8 GB)
echo   02  Ryzen 5  +  RX 7600               (8 GB)
echo.
echo  --- Ryzen 7 (mid) ---
echo   03  Ryzen 7  +  RX 6700 XT            (12 GB)
echo   04  Ryzen 7  +  RX 6800 / 6800 XT     (16 GB)
echo   05  Ryzen 7  +  RX 7900 XT            (20 GB)
echo   06  Ryzen 7  +  RX 7900 XTX           (24 GB)
echo   07  Ryzen 7  +  RX 9070 XT            (16 GB)
echo.
echo  --- Ryzen 9 (12-core) ---
echo   08  Ryzen 9  +  RX 6800 / 6800 XT     (16 GB)
echo   09  Ryzen 9  +  RX 7900 XT            (20 GB)
echo   10  Ryzen 9  +  RX 7900 XTX           (24 GB)
echo   11  Ryzen 9  +  RX 9070 XT            (16 GB)
echo.
echo  --- Ryzen 9 9950X (Zen 5) ---
echo   12  9950X    +  RX 7900 XT            (20 GB)
echo   13  9950X    +  RX 7900 XTX           (24 GB)  ^<-- ideal
echo   14  9950X    +  RX 9070 XT            (16 GB)
echo.
echo  --- Threadripper ---
echo   15  TR 7960X +  RX 7900 XTX
echo   16  TR 7960X +  RX 9070 XT
echo   17  TR 7970X +  RX 7900 XTX
echo   18  TR 7970X +  RX 9070 XT
echo   19  TR 7980X +  RX 7900 XTX
echo   20  TR 7980X +  RX 9070 XT
echo.
echo  --- CPU only (no AMD GPU) ---
echo   21  Ryzen 5  CPU only
echo   22  Ryzen 7  CPU only
echo   23  Ryzen 9  CPU only
echo.
echo   0   Exit
echo.
set /p CHOICE="  Your number: "

if "%CHOICE%"=="0" exit /b 0
if "%CHOICE%"=="1"  set "SLUG=ryzen5-rx6600"       & goto install
if "%CHOICE%"=="01" set "SLUG=ryzen5-rx6600"       & goto install
if "%CHOICE%"=="2"  set "SLUG=ryzen5-rx7600"       & goto install
if "%CHOICE%"=="02" set "SLUG=ryzen5-rx7600"       & goto install
if "%CHOICE%"=="3"  set "SLUG=ryzen7-rx6700xt"     & goto install
if "%CHOICE%"=="03" set "SLUG=ryzen7-rx6700xt"     & goto install
if "%CHOICE%"=="4"  set "SLUG=ryzen7-rx6800xt"     & goto install
if "%CHOICE%"=="04" set "SLUG=ryzen7-rx6800xt"     & goto install
if "%CHOICE%"=="5"  set "SLUG=ryzen7-rx7900xt"     & goto install
if "%CHOICE%"=="05" set "SLUG=ryzen7-rx7900xt"     & goto install
if "%CHOICE%"=="6"  set "SLUG=ryzen7-rx7900xtx"    & goto install
if "%CHOICE%"=="06" set "SLUG=ryzen7-rx7900xtx"    & goto install
if "%CHOICE%"=="7"  set "SLUG=ryzen7-rx9070xt"     & goto install
if "%CHOICE%"=="07" set "SLUG=ryzen7-rx9070xt"     & goto install
if "%CHOICE%"=="8"  set "SLUG=ryzen9-rx6800xt"     & goto install
if "%CHOICE%"=="08" set "SLUG=ryzen9-rx6800xt"     & goto install
if "%CHOICE%"=="9"  set "SLUG=ryzen9-rx7900xt"     & goto install
if "%CHOICE%"=="09" set "SLUG=ryzen9-rx7900xt"     & goto install
if "%CHOICE%"=="10" set "SLUG=ryzen9-rx7900xtx"    & goto install
if "%CHOICE%"=="11" set "SLUG=ryzen9-rx9070xt"     & goto install
if "%CHOICE%"=="12" set "SLUG=ryzen9-9950x-rx7900xt"  & goto install
if "%CHOICE%"=="13" set "SLUG=ryzen9-9950x-rx7900xtx" & goto install
if "%CHOICE%"=="14" set "SLUG=ryzen9-9950x-rx9070xt"  & goto install
if "%CHOICE%"=="15" set "SLUG=tr-7960x-rx7900xtx"  & goto install
if "%CHOICE%"=="16" set "SLUG=tr-7960x-rx9070xt"   & goto install
if "%CHOICE%"=="17" set "SLUG=tr-7970x-rx7900xtx"  & goto install
if "%CHOICE%"=="18" set "SLUG=tr-7970x-rx9070xt"   & goto install
if "%CHOICE%"=="19" set "SLUG=tr-7980x-rx7900xtx"  & goto install
if "%CHOICE%"=="20" set "SLUG=tr-7980x-rx9070xt"   & goto install
if "%CHOICE%"=="21" set "SLUG=cpu-only-ryzen5"     & goto install
if "%CHOICE%"=="22" set "SLUG=cpu-only-ryzen7"     & goto install
if "%CHOICE%"=="23" set "SLUG=cpu-only-ryzen9"     & goto install

echo.
echo  Invalid choice. Try again.
timeout /t 2 >nul
goto menu

:install
set "POW_SRC=%PRESETS%\poworker\%SLUG%.ini"
set "DIA_SRC=%PRESETS%\diaworker\%SLUG%.ini"

if not exist "%POW_SRC%" (
    echo  Preset not found: %POW_SRC%
    pause
    goto menu
)

echo.
echo  Installing preset: %SLUG%
echo.

for %%D in (debug release) do (
    set "OUT=%REPO_ROOT%\target\%%D"
    if exist "!OUT!" (
        copy /Y "%POW_SRC%" "!OUT!\poworker.config.ini" >nul
        if exist "%DIA_SRC%" copy /Y "%DIA_SRC%" "!OUT!\diaworker.config.ini" >nul
        echo   -^> !OUT!\poworker.config.ini
        echo   -^> !OUT!\diaworker.config.ini
    )
)

if not exist "%REPO_ROOT%\target\debug" (
    set "OUT=%REPO_ROOT%\target\debug"
    mkdir "!OUT!" 2>nul
    copy /Y "%POW_SRC%" "!OUT!\poworker.config.ini" >nul
    if exist "%DIA_SRC%" copy /Y "%DIA_SRC%" "!OUT!\diaworker.config.ini" >nul
    echo   -^> !OUT!\poworker.config.ini
)

echo.
echo  Next steps:
echo    1. LIST-OPENCL-DEVICES.bat  — check platform_id / device_ids
echo    2. Edit poworker.config.ini if GPU index is not 0
echo    3. START-AMD-HAC-MINING.bat or START-AMD-HACD-MINING.bat
echo.
echo  Close to main menu, or run another preset.
pause
goto menu