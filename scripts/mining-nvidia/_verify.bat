@echo off
cd /d C:\Users\KQHEX\Documents\hacash-fullnodedev
call "C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
cargo build --release -p x16rs-cuda --features cuda
echo EXITCODE=%ERRORLEVEL%