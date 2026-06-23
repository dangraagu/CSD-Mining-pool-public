@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - install as Windows service (optional)
color 0a

REM ============================================================
REM  install-as-service.bat  (OPTIONAL)
REM
REM  WHAT: registers the CSD pool miner you already installed as a
REM        Windows SERVICE so it auto-starts on boot and auto-restarts
REM        if it crashes - without keeping a console window open.
REM  WHY:  only if you want the miner to run unattended as a service.
REM        The normal launchers (mine-auto.bat) work fine WITHOUT this;
REM        this is purely opt-in. Nothing is installed unless you run it.
REM  NEEDS ADMIN: registering a Windows service requires Administrator
REM        rights, so this script ELEVATES ITSELF (you will see a UAC
REM        prompt). It does NOT run silently or hidden.
REM
REM  It re-uses the SAME miner exe and payout address the other launchers
REM  use (resolved from %LOCALAPPDATA%\csd-pool-miner). The service is the
REM  miner's own  --install-service  (auto-start + crash-restart), then it
REM  is started now with  sc start csd-pool-miner.
REM
REM  To remove the service later: double-click uninstall-service.bat.
REM
REM  Usage:  install-as-service.bat            (auto-detects nvidia/amd/cpu)
REM          install-as-service.bat nvidia     (force a build variant)
REM ============================================================

set "SVC=csd-pool-miner"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"
set "CFG=%DIR%\address.txt"

REM --- pick the build variant (arg overrides auto-detect; same as the installer) ---
set "VARIANT=%~1"
if not defined VARIANT (
  for /f "usebackq delims=" %%i in (`powershell -NoProfile -Command "$n=((Get-CimInstance Win32_VideoController).Name -join ','); if ($n -match 'NVIDIA'){'nvidia'} elseif ($n -match 'AMD' -or $n -match 'Radeon'){'amd'} else {'cpu'}"`) do set "VARIANT=%%i"
)
if not defined VARIANT set "VARIANT=cpu"
set "EXE=csd-pool-miner-%VARIANT%.exe"

REM --- map the build variant to the miner's --backend value ---
REM   cpu    -> cpu      (CPU backend)
REM   nvidia -> cuda     (CUDA backend)
REM   amd    -> opencl   (OpenCL backend)
REM   else   -> auto     (let the miner pick)
set "BACKEND=auto"
if /i "%VARIANT%"=="cpu"    set "BACKEND=cpu"
if /i "%VARIANT%"=="nvidia" set "BACKEND=cuda"
if /i "%VARIANT%"=="amd"    set "BACKEND=opencl"

REM --- resolve the miner exe: the canonical install dir first (what mine-auto.bat
REM     runs), then next to THIS .bat as a fallback. ---
set "BIN="
if exist "%DIR%\%EXE%" set "BIN=%DIR%\%EXE%"
if not defined BIN if exist "%~dp0%EXE%" set "BIN=%~dp0%EXE%"

REM --- am I elevated? (registering a service needs admin) ---
REM  net session only succeeds with admin rights; redirect its output away.
set "ISADMIN=no"
net session >nul 2>&1
if %errorlevel%==0 set "ISADMIN=yes"

echo(
echo  === CSD Pool Miner: install as Windows service (optional) ===
echo  build variant : %VARIANT%   (backend: %BACKEND%)
echo  miner exe     : %BIN%
echo  service name  : %SVC%   (auto-start on boot + auto-restart on crash)
echo  needs admin   : yes  (this window will request elevation)
echo(

REM --- read the saved payout address (does NOT prompt here; the install path
REM     prompts once, in the elevated window, only if it is still missing) ---
set "ADDR="
if exist "%CFG%" set /p ADDR=<"%CFG%"

REM ── TEST HOOK: CSD_SVC_DRYRUN=1 resolves everything and ECHOES the command it
REM    WOULD run (plus the admin-guard decision) to %TEMP%\csd-svc-dryrun.txt, then
REM    exits WITHOUT elevating, calling sc, or launching anything. ZERO effect on a
REM    normal double-click run (the var is unset there). Used by
REM    tests/installer_service_bat.sh to assert the exact built command on real
REM    cmd.exe without needing real admin / a live SCM. A flat GOTO (not a nested
REM    parenthesised block) keeps cmd.exe parsing robust under all line endings.
if "%CSD_SVC_DRYRUN%"=="1" goto :dryrun

REM --- SELF-ELEVATE: if we are not admin, re-launch THIS script elevated (UAC),
REM     forwarding the chosen variant, then exit. The elevated copy continues from
REM     the top and this time the net-session check passes. ---
if "%ISADMIN%"=="no" goto :elevate

goto :do_install

REM ============================================================
REM  Subroutine-style targets (flat control flow for cmd.exe safety)
REM ============================================================

:dryrun
set "DRYOUT=%TEMP%\csd-svc-dryrun.txt"
> "!DRYOUT!" echo ADMIN=%ISADMIN%
>>"!DRYOUT!" echo VARIANT=%VARIANT%
>>"!DRYOUT!" echo BACKEND=%BACKEND%
if not defined BIN (
  >>"!DRYOUT!" echo NO_EXE=1 miner exe %EXE% not found in %DIR% or beside this .bat
  echo [dryrun] miner exe not found - no command built.
  goto :end_ok
)
if not defined ADDR (
  >>"!DRYOUT!" echo NO_ADDR=1 no saved address.txt; the real run prompts for it
  echo [dryrun] no saved address - the real run would prompt.
  goto :end_ok
)
>>"!DRYOUT!" echo INSTALL_CMD="!BIN!" --install-service --address !ADDR! --backend %BACKEND%
echo [dryrun] would run: "!BIN!" --install-service --address !ADDR! --backend %BACKEND%
echo [dryrun] (ADMIN=%ISADMIN%) wrote !DRYOUT!
goto :end_ok

:elevate
echo  Requesting Administrator rights (UAC) to register the service...
powershell -NoProfile -Command "Start-Process -Verb RunAs -FilePath '%~f0' -ArgumentList '%VARIANT%'"
if errorlevel 1 (
  echo(
  echo  [X] Elevation was cancelled or failed. The service was NOT installed.
  echo      Right-click this file and choose "Run as administrator" to try again.
  echo(
  pause
)
endlocal & exit /b

REM ===================== ELEVATED FROM HERE ON =====================
:do_install
if not defined BIN goto :no_exe

REM Prompt for the payout address ONCE if it was never saved (new rig). Saved to
REM the same address.txt the other launchers read, so this only ever happens once.
if defined ADDR goto :have_addr
echo  No saved payout address found.
echo  Enter YOUR addr20 payout address ^(40 hex characters^) - where the pool
echo  sends your mining rewards:
set /p ADDR=^>
if not defined ADDR goto :no_addr
if not exist "%DIR%" mkdir "%DIR%"
> "%CFG%" echo !ADDR!

:have_addr
echo  Registering the service to mine to !ADDR! ...
echo(

REM --- register the auto-start service via the miner's own --install-service.
REM     v0.1.12 --install-service registers an AUTO-START service whose command
REM     line is  "<exe>" --address <addr> --backend <backend> --run-as-service
REM     and sets 3-step crash-restart failure actions (5s/15s/30s) + restart on a
REM     non-zero exit (the GPU-stall watchdog's exit 17). ---
"!BIN!" --install-service --address !ADDR! --backend %BACKEND%
set "RC=!errorlevel!"
if not "!RC!"=="0" goto :install_failed

REM --- start it now (it is also auto-start on the next boot) ---
echo  Service registered. Starting it now...
sc start %SVC% >nul 2>&1
REM sc start returns non-zero if it is ALREADY running (1056) - not a real error.

echo(
echo  [OK] The CSD pool miner is now installed as the Windows service "%SVC%".
echo       * it AUTO-STARTS on every boot,
echo       * it AUTO-RESTARTS if it crashes or the GPU stalls,
echo       * it runs in the background (no console window needed),
echo       * mining to: !ADDR!  (backend: %BACKEND%).
echo(
echo  Verify it:     sc query %SVC%          (should show STATE: 4 RUNNING)
echo  Stop it once:  sc stop %SVC%
echo  Remove it:     double-click uninstall-service.bat
echo(
echo  This service is OPTIONAL. If you would rather just run the miner in a
echo  window, remove it with uninstall-service.bat and use mine-auto.bat instead.
echo(
pause
endlocal & exit /b 0

:no_exe
echo(
echo  [X] Could not find the miner exe (%EXE%).
echo      Run install-csd-miner.bat first (it downloads the miner into
echo      %DIR%), then run this again.
echo      Tip: to force a different build:  install-as-service.bat nvidia ^| amd ^| cpu
echo(
pause
endlocal & exit /b 1

:no_addr
echo  [X] No address entered. Re-run and provide your addr20. The service was NOT installed.
pause
endlocal & exit /b 1

:install_failed
echo(
echo  [X] Registering the service FAILED ^(exit !RC!^).
echo      A KNOWN narrow edge: the miner can fail AFTER creating the service
echo      while setting its restart/failure actions, leaving a REGISTERED but
echo      UNSTARTED service. To clean that up and try again:
echo         1. double-click  uninstall-service.bat   ^(removes any orphan^)
echo         2. then run this  install-as-service.bat  again
echo      ^(Also make sure no other CSD service install is in progress.^)
echo(
pause
endlocal & exit /b !RC!

:end_ok
endlocal & exit /b 0
