@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - one-click installer
color 0a

REM ============================================================
REM  CSD Pool Miner - all-in-one installer for Windows.
REM  Double-click. It will:
REM    1. Detect your GPU (NVIDIA / AMD) or fall back to CPU.
REM    2. Install the Microsoft VC++ runtime via winget if missing.
REM    3. Download the matching prebuilt miner from GitHub Releases.
REM    4. Ask for your addr20 payout address once (and remember it).
REM    5. Start mining to the pool.
REM  Override detection:  install-csd-miner.bat nvidia ^| amd ^| cpu
REM  GPU drivers are NOT installed here - the GPU builds need your
REM  vendor driver already present; otherwise use the cpu build.
REM ============================================================

set "REPO=dangraagu/CSD-Mining-pool-public"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"
set "CFG=%DIR%\address.txt"
if not exist "%DIR%" mkdir "%DIR%"

echo(
echo  === CSD Pool Miner installer ===
echo(

REM --- 1. Pick the build variant (arg overrides auto-detect) ---
set "VARIANT=%~1"
if not defined VARIANT (
  REM Pipe-free PowerShell (no '|' to mis-escape inside the for/f backticks):
  REM use .Name instead of "| Select-Object" and "-or" instead of the regex "|".
  for /f "usebackq delims=" %%i in (`powershell -NoProfile -Command "$n=((Get-CimInstance Win32_VideoController).Name -join ','); if ($n -match 'NVIDIA'){'nvidia'} elseif ($n -match 'AMD' -or $n -match 'Radeon'){'amd'} else {'cpu'}"`) do set "VARIANT=%%i"
)
if not defined VARIANT set "VARIANT=cpu"
echo Selected build: %VARIANT%

set "EXE=csd-pool-miner-%VARIANT%.exe"
set "BIN=%DIR%\%EXE%"
set "URL=https://github.com/%REPO%/releases/latest/download/%EXE%"

REM --- 2. VC++ runtime via winget (best-effort; skipped if absent) ---
where winget >nul 2>&1
if !errorlevel!==0 (
  winget list --id Microsoft.VCRedist.2015+.x64 -e >nul 2>&1
  if !errorlevel! NEQ 0 (
    echo Installing Microsoft VC++ runtime...
    winget install --id Microsoft.VCRedist.2015+.x64 -e --silent --accept-source-agreements --accept-package-agreements
  ) else (
    echo VC++ runtime already present.
  )
) else (
  echo winget not found - skipping VC++ check ^(usually already installed^).
)

REM --- 3. Download the matching miner ---
echo Downloading %EXE% ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -o "%BIN%" "%URL%"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '%URL%' -OutFile '%BIN%' -UseBasicParsing } catch { exit 1 }"
)
if !errorlevel! NEQ 0 (
  echo(
  echo [X] Download failed. Either no release is published yet, the
  echo     '%VARIANT%' build isn't in the latest release, or no network.
  echo     Releases: https://github.com/%REPO%/releases/latest
  echo     Tip: try another build, e.g.  install-csd-miner.bat cpu
  echo(
  pause
  exit /b 1
)

REM --- 3b. Also fetch the multi-GPU + auto-update launchers next to this file ---
echo Fetching the multi-GPU / auto-update launchers ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -s -o "%~dp0mine-all-gpus.bat" "https://raw.githubusercontent.com/%REPO%/main/mine-all-gpus.bat"
  curl -L -f -s -o "%~dp0mine-auto.bat" "https://raw.githubusercontent.com/%REPO%/main/mine-auto.bat"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/%REPO%/main/mine-all-gpus.bat' -OutFile '%~dp0mine-all-gpus.bat' -UseBasicParsing; Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/%REPO%/main/mine-auto.bat' -OutFile '%~dp0mine-auto.bat' -UseBasicParsing } catch {}"
)
echo   - mine-all-gpus.bat  = mine on ALL GPUs at once
echo   - mine-auto.bat      = all GPUs + auto-update (recommended for 24/7)

REM --- 4. addr20 payout address: prompt once, remember thereafter ---
set "ADDR="
if exist "%CFG%" set /p ADDR=<"%CFG%"
if not defined ADDR (
  echo(
  echo Enter YOUR addr20 payout address ^(40 hex characters^) - where the
  echo pool sends your mining rewards:
  set /p ADDR=^>
  > "%CFG%" echo !ADDR!
)
if not defined ADDR (
  echo [X] No address entered. Re-run and provide your addr20.
  pause
  exit /b 1
)

REM --- 5. Mine ---
echo(
echo Starting %VARIANT% miner. Payout address: !ADDR!
echo ^(Change it later by deleting: %CFG%^)
echo Press Ctrl+C to stop.
echo(
"%BIN%" --address !ADDR!

echo(
echo Miner stopped.
pause
endlocal
