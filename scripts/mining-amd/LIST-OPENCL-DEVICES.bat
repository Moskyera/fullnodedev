@echo off
setlocal
set "REPO_ROOT=%~dp0..\.."

if exist "%REPO_ROOT%\target\release\list_opencl.exe" (
    "%REPO_ROOT%\target\release\list_opencl.exe"
    goto :done
)
if exist "%REPO_ROOT%\target\debug\list_opencl.exe" (
    "%REPO_ROOT%\target\debug\list_opencl.exe"
    goto :done
)

echo list_opencl.exe not found. Run BUILD-AMD-MINER.bat first.
pause
exit /b 1

:done
echo.
pause
exit /b 0