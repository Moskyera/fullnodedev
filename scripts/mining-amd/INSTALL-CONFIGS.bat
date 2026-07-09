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
echo  Easiest: CONFIGURE-MINING.bat — type your CPU and GPU (e.g. 9950x + 7900xtx)
echo  Or: PICK-PRESET.bat — pick from numbered list (PRESETS-INDEX.txt)
echo.
echo  Then: LIST-OPENCL-DEVICES.bat — set platform_id / device_ids
echo.
pause
exit /b 0