@echo off
setlocal EnableDelayedExpansion

set "REPO_ROOT=%~dp0..\.."
set "DEBUG_DIR=%REPO_ROOT%\target\debug"
set "RELEASE_DIR=%REPO_ROOT%\target\release"
set "SCRIPT_DIR=%~dp0"

if not exist "%DEBUG_DIR%" mkdir "%DEBUG_DIR%"

for %%D in (debug release) do (
    set "OUT=%REPO_ROOT%\target\%%D"
    if exist "!OUT!" (
        copy /Y "%SCRIPT_DIR%poworker.amd.ini.example" "!OUT!\poworker.config.ini" >nul
        copy /Y "%SCRIPT_DIR%diaworker.amd.ini.example" "!OUT!\diaworker.config.ini" >nul
        echo Installed configs in !OUT!
    )
)

echo.
echo  Tune [gpu] platform_id and device_ids after LIST-OPENCL-DEVICES.bat
echo  Tune supervene to Ryzen logical cores you want for CPU mining
echo.
pause
exit /b 0