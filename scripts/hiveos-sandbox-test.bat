@echo off
rem hiveos-sandbox-test.bat — double-click wrapper for hiveos-sandbox-test.sh.
rem Runs a Linux csd-pool-miner binary inside the csd-hiveos-sandbox WSL
rem distro (Ubuntu 18.04 / glibc 2.27 = stock-HiveOS floor) and prints
rem PASS/FAIL per check. Pre-tag gate for every Linux fleet release.
rem
rem Usage:
rem   double-click            -> tests target\x86_64-unknown-linux-gnu\release\csd-gpu-miner
rem   drag a binary onto it   -> tests that binary
rem   hiveos-sandbox-test.bat <path-to-linux-binary>

setlocal
set "SCRIPT_DIR=%~dp0"

rem Prefer Git Bash (handles the .sh natively on the Windows side).
set "BASH_EXE="
if exist "C:\Program Files\Git\bin\bash.exe" set "BASH_EXE=C:\Program Files\Git\bin\bash.exe"
if not defined BASH_EXE if exist "C:\Program Files (x86)\Git\bin\bash.exe" set "BASH_EXE=C:\Program Files (x86)\Git\bin\bash.exe"
if not defined BASH_EXE (
    for %%B in (bash.exe) do set "BASH_EXE=%%~$PATH:B"
)
if not defined BASH_EXE (
    echo FATAL: no bash.exe found ^(install Git for Windows^)
    pause
    exit /b 1
)

if "%~1"=="" (
    "%BASH_EXE%" "%SCRIPT_DIR%hiveos-sandbox-test.sh"
) else (
    "%BASH_EXE%" "%SCRIPT_DIR%hiveos-sandbox-test.sh" "%~1"
)
set "RC=%ERRORLEVEL%"

echo.
if "%RC%"=="0" (
    echo RESULT: PASS - safe on HiveOS-era glibc
) else (
    echo RESULT: FAIL - DO NOT TAG ^(exit %RC%^)
)
pause
exit /b %RC%
