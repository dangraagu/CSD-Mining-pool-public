@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - uninstall the Windows service
color 0a

REM ============================================================
REM  uninstall-service.bat  (OPTIONAL)
REM
REM  WHAT: stops and REMOVES the "csd-pool-miner" Windows service that
REM        install-as-service.bat created. After this the miner no longer
REM        runs on boot; use mine-auto.bat to mine in a window instead.
REM  WHY:  run this if you no longer want the miner running as a service,
REM        OR to clean up after a failed install-as-service.bat (a rare
REM        SCM error can leave a registered-but-unstarted service - this
REM        removes that orphan cleanly so you can retry the install).
REM  NEEDS ADMIN: removing a Windows service requires Administrator rights,
REM        so this script ELEVATES ITSELF (you will see a UAC prompt).
REM
REM  Usage:  uninstall-service.bat            (auto-detects nvidia/amd/cpu)
REM          uninstall-service.bat nvidia     (force a build variant)
REM ============================================================

set "SVC=csd-pool-miner"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"

REM --- pick the build variant (only used to locate the exe for the miner's own
REM     --uninstall-service; sc delete is variant-independent) ---
set "VARIANT=%~1"
if not defined VARIANT (
  for /f "usebackq delims=" %%i in (`powershell -NoProfile -Command "$n=((Get-CimInstance Win32_VideoController).Name -join ','); if ($n -match 'NVIDIA'){'nvidia'} elseif ($n -match 'AMD' -or $n -match 'Radeon'){'amd'} else {'cpu'}"`) do set "VARIANT=%%i"
)
if not defined VARIANT set "VARIANT=cpu"
set "EXE=csd-pool-miner-%VARIANT%.exe"

REM --- resolve the miner exe (canonical install dir first, then beside this .bat) ---
set "BIN="
if exist "%DIR%\%EXE%" set "BIN=%DIR%\%EXE%"
if not defined BIN if exist "%~dp0%EXE%" set "BIN=%~dp0%EXE%"

REM --- am I elevated? ---
set "ISADMIN=no"
net session >nul 2>&1
if %errorlevel%==0 set "ISADMIN=yes"

echo(
echo  === CSD Pool Miner: uninstall the Windows service ===
echo  service name : %SVC%
echo  miner exe    : %BIN%
echo  needs admin  : yes  (this window will request elevation)
echo(

REM ── TEST HOOK: CSD_SVC_DRYRUN=1 echoes the command it WOULD run (plus the
REM    admin-guard decision) to %TEMP%\csd-svc-dryrun.txt, then exits WITHOUT
REM    elevating, calling sc, or removing anything. ZERO effect on a normal run.
if "%CSD_SVC_DRYRUN%"=="1" goto :dryrun

REM --- SELF-ELEVATE if not admin (forward the variant), then exit. ---
if "%ISADMIN%"=="no" goto :elevate

goto :do_uninstall

REM ============================================================
REM  Flat targets (cmd.exe-safe control flow)
REM ============================================================

:dryrun
set "DRYOUT=%TEMP%\csd-svc-dryrun.txt"
> "!DRYOUT!" echo ADMIN=%ISADMIN%
>>"!DRYOUT!" echo VARIANT=%VARIANT%
if not defined BIN goto :dryrun_no_exe
>>"!DRYOUT!" echo UNINSTALL_CMD="!BIN!" --uninstall-service
echo [dryrun] would run: "!BIN!" --uninstall-service  (fallback: sc delete %SVC%)
goto :end_ok
:dryrun_no_exe
>>"!DRYOUT!" echo NO_EXE=1 will fall back to: sc delete %SVC%
echo [dryrun] miner exe not found - would fall back to: sc delete %SVC%
goto :end_ok

:elevate
echo  Requesting Administrator rights (UAC) to remove the service...
powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0' -ArgumentList '%VARIANT%'"
if errorlevel 1 (
  echo(
  echo  [X] Elevation was cancelled or failed. The service was NOT removed.
  echo      Right-click this file and choose "Run as administrator" to try again.
  echo(
  pause
)
endlocal & exit /b

REM ===================== ELEVATED FROM HERE ON =====================
:do_uninstall
REM --- 1. stop the service if it is running (ignore "not running" errors) ---
echo  Stopping the service (if running)...
sc stop %SVC% >nul 2>&1

REM --- 2. remove it. Prefer the miner's own --uninstall-service (it stops + deletes
REM     via the SCM the same way it installed). If the exe is missing or that fails,
REM     fall back to a plain  sc delete  so an orphan is always removable. ---
set "REMOVED=0"
if not defined BIN goto :sc_delete
"!BIN!" --uninstall-service
if not errorlevel 1 set "REMOVED=1"
if "!REMOVED!"=="1" goto :report

:sc_delete
echo  Falling back to: sc delete %SVC%
sc delete %SVC% >nul 2>&1
if not errorlevel 1 set "REMOVED=1"
if "!REMOVED!"=="1" goto :report
REM sc delete returns 1060 if the service does not exist - that is the goal state,
REM so treat "not installed" as success (idempotent removal).
sc query %SVC% >nul 2>&1
if errorlevel 1 set "REMOVED=1"

:report
echo(
if "!REMOVED!"=="1" (
  echo  [OK] The "%SVC%" Windows service has been removed.
  echo       The miner no longer starts on boot. To mine in a window, run
  echo       mine-auto.bat ^(no admin needed^).
) else (
  echo  [!] Could not confirm removal of "%SVC%". Check it with:  sc query %SVC%
  echo      If it still shows, run this again as administrator, or:  sc delete %SVC%
)
echo(
pause
endlocal & exit /b 0

:end_ok
endlocal & exit /b 0
