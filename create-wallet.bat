@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - create wallet
color 0b

REM ============================================================
REM  CSD Pool Miner - one-click WALLET CREATOR for Windows.
REM  Double-click. It will:
REM    1. Make sure the miner binary is present (download if not).
REM    2. Generate a fresh CSD wallet (keypair + addr20) LOCALLY.
REM    3. Print your new payout address + save the key to
REM       csd-wallet.txt next to this script.
REM  The private key is created on THIS machine and is NEVER sent
REM  anywhere. BACK IT UP - losing it loses the coins.
REM
REM  We use the CPU build for this: generating a key needs no GPU
REM  and no driver, so it works on any machine.
REM ============================================================

set "REPO=dangraagu/CSD-Mining-pool-public"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"
if not exist "%DIR%" mkdir "%DIR%"

REM Key generation is CPU-only work; the cpu build runs everywhere with no
REM GPU/driver requirement. (Same asset naming as install-csd-miner.bat.)
set "EXE=csd-pool-miner-cpu.exe"
set "BIN=%DIR%\%EXE%"
set "URL=https://github.com/%REPO%/releases/latest/download/%EXE%"

echo(
echo  === CSD Pool Miner - create a new wallet ===
echo(

REM --- 1. Ensure the miner binary is present (download if missing) ---
if exist "%BIN%" (
  echo Miner binary already present: %BIN%
) else (
  echo Downloading the miner ^(%EXE%^) ...
  where curl >nul 2>&1
  if !errorlevel!==0 (
    curl -L -f -o "%BIN%" "%URL%"
  ) else (
    powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '%URL%' -OutFile '%BIN%' -UseBasicParsing } catch { exit 1 }"
  )
  if !errorlevel! NEQ 0 (
    echo(
    echo [X] Download failed. Either no release is published yet, the
    echo     cpu build isn't in the latest release, or there is no network.
    echo     Releases: https://github.com/%REPO%/releases/latest
    echo(
    pause
    exit /b 1
  )
)

REM --- 2. Generate the wallet (writes csd-wallet.txt in THIS folder) ---
echo(
echo Generating your new CSD wallet ^(this stays on your machine^)...
echo(
pushd "%~dp0"
"%BIN%" newwallet
set "RC=!errorlevel!"
popd

if !RC! NEQ 0 (
  echo(
  echo [X] Wallet generation failed ^(exit code !RC!^).
  pause
  exit /b 1
)

echo(
echo  ------------------------------------------------------------
echo  Your new payout ADDRESS is shown above.
echo  Your private key was saved to:  "%~dp0csd-wallet.txt"
echo(
echo  *** BACK UP csd-wallet.txt NOW. ***
echo  If you lose the private key, the coins are GONE FOREVER.
echo  Only ever share the ADDRESS - never the private key.
echo(
echo  Next: start mining with install-csd-miner.bat and paste the
echo  address when asked, or run:  csd-pool-miner --address ^<addr^>
echo  ------------------------------------------------------------
echo(
pause
endlocal
