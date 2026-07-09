@echo off
:: Wrapper: run the main setup from repo root (works for dev and release zip).
set "ROOT=%~dp0..\.."
if exist "%ROOT%SETUP.bat" (
    call "%ROOT%SETUP.bat"
    exit /b %ERRORLEVEL%
)
echo SETUP.bat not found at repo root.
pause
exit /b 1