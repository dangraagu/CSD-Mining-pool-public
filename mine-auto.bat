@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - auto-update (all GPUs)
color 0a

REM ============================================================
REM  Self-updating, multi-GPU launcher. Leave this window open.
REM   * Runs one miner instance per GPU (each --device i, all to
REM     your address) for the biggest combined hashrate.
REM   * Every CHECK_MIN minutes it asks GitHub for the latest
REM     release; when a NEW version is published it stops the
REM     miners, downloads it, and restarts them automatically.
REM   * If a miner window dies, it gets restarted on the next check.
REM  Build (default OpenCL/amd = NVIDIA+AMD on just the driver):
REM     mine-auto.bat nvidia
REM ============================================================

set "REPO=dangraagu/CSD-Mining-pool-public"
set "VARIANT=%~1"
if not defined VARIANT set "VARIANT=amd"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"
set "EXE=csd-pool-miner-%VARIANT%.exe"
set "BIN=%DIR%\%EXE%"
set "CFG=%DIR%\address.txt"
set "CHECK_MIN=15"
if not exist "%DIR%" mkdir "%DIR%"

echo(
echo  === CSD Pool Miner - auto-update (build: %VARIANT%) ===
echo(

REM --- payout address (reuse the saved one, else prompt) ---
set "ADDR="
if exist "%CFG%" set /p ADDR=<"%CFG%"
if not defined ADDR (
  set /p ADDR=Enter your addr20 payout address ^(40 hex^):
  > "%CFG%" echo !ADDR!
)
if not defined ADDR ( echo [X] No address entered. & pause & exit /b 1 )

REM --- count GPUs (pipe-free + array-safe so the for/f works) ---
set "NGPU=1"
for /f "usebackq delims=" %%n in (`powershell -NoProfile -Command "$g=@((Get-CimInstance Win32_VideoController).Name); $c=0; foreach($n in $g){ if($n -match 'NVIDIA' -or $n -match 'AMD' -or $n -match 'Radeon'){$c++} }; $c"`) do set "NGPU=%%n"
if !NGPU! LSS 1 set "NGPU=1"
set /a LAST=!NGPU!-1
echo Rig has !NGPU! GPU(s). Mining to !ADDR!.
echo Auto-checking GitHub for updates every %CHECK_MIN% min. Keep this open.
echo(

set "INSTALLED=none"

:loop
REM --- latest published version tag (no pipe -> safe in for/f) ---
set "LATEST="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "try { (Invoke-RestMethod -Uri 'https://api.github.com/repos/%REPO%/releases/latest' -Headers @{'User-Agent'='csd-miner'}).tag_name } catch { '' }"`) do set "LATEST=%%v"

if defined LATEST if not "!LATEST!"=="!INSTALLED!" (
  echo [%time%] update: !INSTALLED! -^> !LATEST!  ^(stopping, downloading, restarting^)
  taskkill /IM "%EXE%" /F >nul 2>&1
  curl -L -f -o "%BIN%" "https://github.com/%REPO%/releases/latest/download/%EXE%"
  if !errorlevel!==0 (
    set "INSTALLED=!LATEST!"
    for /l %%i in (0,1,!LAST!) do start "CSD miner GPU %%i (!LATEST!)" "%BIN%" --address !ADDR! --device %%i --log-dir "%DIR%\gpu%%i-log"
    echo [%time%] now mining !LATEST! on !NGPU! GPU(s).
  ) else (
    echo [%time%] download failed; keeping current, will retry.
  )
)

REM --- restart miners if none are running (window closed / crashed) ---
tasklist /FI "IMAGENAME eq %EXE%" 2>nul | find /I "%EXE%" >nul
if errorlevel 1 if not "!INSTALLED!"=="none" (
  echo [%time%] miners not running - restarting on !NGPU! GPU(s)
  for /l %%i in (0,1,!LAST!) do start "CSD miner GPU %%i" "%BIN%" --address !ADDR! --device %%i --log-dir "%DIR%\gpu%%i-log"
)

set /a WAIT=%CHECK_MIN%*60
powershell -NoProfile -Command "Start-Sleep -Seconds !WAIT!"
goto loop
