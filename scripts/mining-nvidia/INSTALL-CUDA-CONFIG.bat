@echo off
setlocal
set "SCRIPT_DIR=%~dp0"
set "REPO_ROOT=%SCRIPT_DIR%..\.."

for %%D in (release debug) do (
    if exist "%REPO_ROOT%\target\%%D\poworker.exe" (
        copy /Y "%SCRIPT_DIR%poworker.cuda.ini.example" "%REPO_ROOT%\target\%%D\poworker.config.ini" >nul
        echo [OK] target\%%D\poworker.config.ini
    )
)

if not exist "%REPO_ROOT%\target\release\poworker.exe" (
    if not exist "%REPO_ROOT%\target\debug\poworker.exe" (
        echo poworker.exe not found. Run BUILD-CUDA-MINER.bat first.
        exit /b 1
    )
)

echo.
echo Edit connect= to your fullnode RPC, then START-CUDA-MINING.bat
endlocal