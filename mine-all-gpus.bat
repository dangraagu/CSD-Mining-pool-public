@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - all GPUs + auto-update
color 0a

REM --- What this is -------------------------------------------------------
REM Opt-in CSD miner launcher: mines on THIS machine, to YOUR own payout
REM address, only while you choose to run it. Not silent or hidden, and does
REM not install or run itself on anyone else's computer. Standard pool miner
REM for the public Compute Substrate (CSD) chain. See README.
REM ------------------------------------------------------------------------

REM ============================================================
REM  Runs ONE miner instance per GPU for the biggest combined
REM  hashrate. Each instance mines the SAME payout address on a
REM  different --device; the pool sums their shares.
REM  Default = the OpenCL ("amd") build, which drives NVIDIA and
REM  AMD GPUs with just the vendor driver (no CUDA toolkit needed).
REM  Use the CUDA build instead with:  mine-all-gpus.bat nvidia
REM
REM  AUTO-UPDATE (fleet, no clawback - FAIL-SAFE IS RULE #1):
REM  This launcher also keeps itself current. A poll loop checks
REM  GitHub every CHECK_MIN minutes; a newer release is gated through
REM  the SAME three checks mine-auto.bat uses - numeric semver compare
REM  (the binary's own check-update), download to a TEMP path, and
REM  SHA-256 verify against the release SHA256SUMS - BEFORE an atomic
REM  swap. Only after a verified swap are the per-GPU miners restarted.
REM  ANY update failure (no network, rate-limit, SHA mismatch, partial
REM  download) falls through and the EXISTING miner windows keep running.
REM
REM  Env knobs (all optional):
REM     CHECK_MIN   update-poll period in minutes   (default 15)
REM ============================================================

set "REPO=dangraagu/CSD-Mining-pool-public"
set "VARIANT=%~1"
if not defined VARIANT set "VARIANT=amd"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"
set "EXE=csd-pool-miner-%VARIANT%.exe"
set "BIN=%DIR%\%EXE%"
set "CFG=%DIR%\address.txt"
if not defined CHECK_MIN set "CHECK_MIN=15"
if not exist "%DIR%" mkdir "%DIR%"

echo(
echo  === CSD Pool Miner - all GPUs + auto-update (build: %VARIANT%) ===
echo(

REM --- 1. download the latest binary (verified) ---
REM Download to a TEMP path, look up the expected SHA-256 from the release
REM SHA256SUMS, verify with the trusted binary's verify-file (or fall through if
REM no checksums published), then atomically move it onto %BIN%. On any failure
REM we keep/run whatever %BIN% already exists (FAIL-SAFE).
call :update_once "first"
if not exist "%BIN%" (
  echo [X] Download/verify failed and no usable binary is present.
  echo     Releases: https://github.com/%REPO%/releases/latest
  pause
  exit /b 1
)

REM --- 2. payout address (reuse the saved one, else prompt) ---
set "ADDR="
if exist "%CFG%" set /p ADDR=<"%CFG%"
if not defined ADDR (
  set /p ADDR=Enter your addr20 payout address ^(40 hex^):
  > "%CFG%" echo !ADDR!
)
if not defined ADDR ( echo [X] No address entered. & pause & exit /b 1 )

REM --- 3. count GPUs (pipe-free + array-safe so the for/f works) ---
set "NGPU="
for /f "usebackq delims=" %%n in (`powershell -NoProfile -Command "$g=@((Get-CimInstance Win32_VideoController).Name); $c=0; foreach($n in $g){ if($n -match 'NVIDIA' -or $n -match 'AMD' -or $n -match 'Radeon'){$c++} }; $c"`) do set "NGPU=%%n"
if not defined NGPU set "NGPU=1"
if !NGPU! LSS 1 set "NGPU=1"
set /a LAST=!NGPU!-1
echo Detected !NGPU! GPU(s). Launching one miner per GPU to !ADDR! ...
echo Auto-checking GitHub for updates every %CHECK_MIN% min (verified swap, then restart). Keep this open.
echo(

REM --- 4. spawn one instance per GPU device (0 .. NGPU-1) ---
call :start_all

echo(
echo Launched !NGPU! window(s), one per GPU, all mining to !ADDR!.
echo Each window shows its own hashrate. This window stays open and auto-updates.
echo A window that closes instantly = that index had no usable GPU (harmless).
echo(

REM --- 5. auto-update poll loop ---
REM Read the installed version once for the semver compare, then poll forever.
set "INSTALLED="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "try { (& '%BIN%' --version) -join ' ' } catch { '' }"`) do set "INSTALLED=%%v"
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "$m=[regex]::Match('!INSTALLED!','[0-9]+\.[0-9]+\.[0-9]+'); if($m.Success){$m.Value}else{'none'}"`) do set "INSTALLED=%%v"
if not defined INSTALLED set "INSTALLED=none"

:loop
set /a SLEEP_SEC=%CHECK_MIN%*60
powershell -NoProfile -Command "Start-Sleep -Seconds %SLEEP_SEC%"
call :update_once "poll"
goto loop

REM ============================================================
REM  Subroutines
REM ============================================================

REM :start_all  - spawn one miner window per GPU device index (0 .. LAST).
:start_all
for /l %%i in (0,1,!LAST!) do (
  echo   GPU %%i  -^>  new window
  start "CSD miner - GPU %%i (!INSTALLED!)" "%BIN%" --address !ADDR! --device %%i --log-dir "%DIR%\gpu%%i-log"
)
goto :eof

REM :stop_all  - close all per-GPU miner processes by image name.
:stop_all
taskkill /IM "%EXE%" /F >nul 2>&1
goto :eof

REM :update_once <mode>  - check for a newer release; on a newer+VERIFIED build,
REM atomically swap it in. In "poll" mode it also restarts all GPU windows after
REM a successful swap. In "first" mode it just stages the initial binary. ANY
REM failure leaves the running binary/miners untouched (FAIL-SAFE).
:update_once
set "MODE=%~1"

REM Latest published release tag (no pipe -> safe in for/f).
set "LATEST="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "try { (Invoke-RestMethod -Uri 'https://api.github.com/repos/%REPO%/releases/latest' -Headers @{'User-Agent'='csd-miner'}).tag_name } catch { '' }"`) do set "LATEST=%%v"
if not defined LATEST goto :eof

REM Decide whether LATEST is newer than INSTALLED. Prefer the binary's OWN
REM check-update (numeric semver). If the binary is missing or predates the
REM subcommand, fall back to a plain string inequality. In "first" mode INSTALLED
REM may be empty -> treat as "none" so we always stage something.
if not defined INSTALLED set "INSTALLED=none"
REM For the string fallback only, compare a 'v'-stripped tag so a bare INSTALLED
REM semver ("0.1.10") is not seen as different from the tag ("v0.1.10"), which
REM would re-download every poll. The binary's own check-update already handles
REM the 'v' prefix, so this matters only when %BIN% predates that subcommand.
set "LATEST_CMP=!LATEST!"
if "!LATEST_CMP:~0,1!"=="v" set "LATEST_CMP=!LATEST_CMP:~1!"
set "DOUPDATE=0"
if exist "%BIN%" (
  "%BIN%" check-update --help >nul 2>&1
  if !errorlevel!==0 (
    "%BIN%" check-update --current "!INSTALLED!" --latest "!LATEST!" >nul 2>&1
    if !errorlevel!==0 ( set "DOUPDATE=1" )
  ) else (
    if not "!LATEST_CMP!"=="!INSTALLED!" set "DOUPDATE=1"
  )
) else (
  set "DOUPDATE=1"
)
if "!DOUPDATE!"=="0" goto :eof

echo [%time%] update: !INSTALLED! -^> !LATEST!  ^(verify, then swap^)

REM 1. Download the new binary to a TEMP path - NEVER onto the live %BIN%.
set "NEWBIN=%BIN%.new"
if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
curl -L -f -o "!NEWBIN!" "https://github.com/%REPO%/releases/latest/download/%EXE%"
if not !errorlevel!==0 (
  echo [%time%] download failed; keeping current, will retry.
  if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
  goto :eof
)

REM 2. Look up the expected SHA-256 from the release SHA256SUMS.
set "WANT="
set "SUMS=%DIR%\SHA256SUMS.allgpus.tmp"
if exist "!SUMS!" del /f /q "!SUMS!" >nul 2>&1
curl -L -f -s -o "!SUMS!" "https://github.com/%REPO%/releases/latest/download/SHA256SUMS"
if exist "!SUMS!" (
  for /f "usebackq tokens=1,2" %%a in (`findstr /i /e /c:" %EXE%" /c:"*%EXE%" "!SUMS!"`) do set "WANT=%%a"
  del /f /q "!SUMS!" >nul 2>&1
)

REM 3. Verify before swapping. Prefer the TRUSTED running %BIN%'s verify-file -
REM    never let the just-downloaded staged binary verify itself. If %BIN%
REM    predates verify-file, FAIL CLOSED. If no SHA256SUMS was published
REM    (pre-P4 release), log and accept the download rather than blocking.
if defined WANT (
  if exist "%BIN%" (
    "%BIN%" verify-file --help >nul 2>&1
    if !errorlevel!==0 (
      "%BIN%" verify-file "!NEWBIN!" "!WANT!" >nul 2>&1
      if not !errorlevel!==0 (
        echo [%time%] [X] SHA-256 verify FAILED for %EXE% - discarding it, keeping the running binary.
        del /f /q "!NEWBIN!" >nul 2>&1
        goto :eof
      )
    ) else (
      echo [%time%] [X] cannot verify ^(running binary predates verify-file^) - refusing. Install v0.1.8+ once, then auto-update verifies.
      del /f /q "!NEWBIN!" >nul 2>&1
      goto :eof
    )
  ) else (
    REM First run, no prior binary to verify with: accept the staged download so
    REM we have something to run; subsequent updates verify against it.
    echo [%time%] [!] no prior binary to verify with ^(first install^) - accepting the download.
  )
) else (
  echo [%time%] [!] no SHA256SUMS published for this release ^(or %EXE% not listed^) - skipping integrity verify ^(pre-P4 release^).
)

REM 4. Verified (or verify intentionally skipped): swap atomically. In poll mode,
REM    stop the running miners first and restart them on the new binary.
if /i "!MODE!"=="poll" call :stop_all
move /Y "!NEWBIN!" "%BIN%" >nul
if not !errorlevel!==0 (
  echo [%time%] [X] could not swap in the new binary; keeping current.
  if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
  goto :eof
)
set "INSTALLED=!LATEST!"
REM Normalise INSTALLED to a bare semver for the next compare.
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "$m=[regex]::Match('!INSTALLED!','[0-9]+\.[0-9]+\.[0-9]+'); if($m.Success){$m.Value}else{'!INSTALLED!'}"`) do set "INSTALLED=%%v"
if /i "!MODE!"=="poll" (
  call :start_all
  echo [%time%] now mining !INSTALLED! on !NGPU! GPU(s) (build: %VARIANT%).
)
goto :eof
