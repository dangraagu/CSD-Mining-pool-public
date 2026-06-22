@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - auto-update (all GPUs)
color 0a

REM --- What this is -------------------------------------------------------
REM Opt-in CSD miner launcher: mines on THIS machine, to YOUR own payout
REM address, only while you choose to run it. Not silent or hidden, and does
REM not install or run itself on anyone else's computer. Standard pool miner
REM for the public Compute Substrate (CSD) chain. See README.
REM ------------------------------------------------------------------------

REM ============================================================
REM  Self-updating, multi-GPU launcher. Leave this window open.
REM   * Runs one miner instance per GPU (each --device i, all to
REM     your address) for the biggest combined hashrate.
REM   * Checks GitHub for the latest release every CHECK_MIN
REM     minutes. A new version is gated through THREE checks
REM     before it ever runs (P4 hardening):
REM       1. semver compare (the miner's own `check-update`, so
REM          0.1.10 is correctly newer than 0.1.9 - a string
REM          compare got this wrong),
REM       2. download to a TEMP path "%BIN%.new" (NEVER onto the
REM          live running binary - the old code curled straight
REM          onto %BIN% and a partial download corrupted it),
REM       3. SHA-256 verify against the release SHA256SUMS (the
REM          miner's own `verify-file`) BEFORE the atomic swap.
REM     A failed verify deletes the temp and keeps the running
REM     binary; the rig never executes an unverified download.
REM   * Liveness is checked on a SHORT cadence (LIVE_SEC),
REM     decoupled from the slow update poll, with ESCALATING
REM     BACKOFF so a crash-looping rig doesn't hammer.
REM  Build (default OpenCL/amd = NVIDIA+AMD on just the driver):
REM     mine-auto.bat nvidia
REM
REM  Env knobs (all optional):
REM     CHECK_MIN     update-poll period in minutes      (default 15)
REM     LIVE_SEC      liveness-check period in seconds    (default 30)
REM     MAX_RESTARTS  rapid restarts before backing off   (default 5)
REM     CSD_GPU_IDS   comma list of GPU ids to mine, e.g
REM                   "0,2" to skip card 1 (default: all cards)
REM     CSD_ON_CRASH  path to a .bat run once when the
REM                   restart cap is hit (driver reset, etc.)
REM ============================================================

set "REPO=dangraagu/CSD-Mining-pool-public"
set "VARIANT=%~1"
if not defined VARIANT set "VARIANT=amd"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"
set "EXE=csd-pool-miner-%VARIANT%.exe"
set "BIN=%DIR%\%EXE%"
set "CFG=%DIR%\address.txt"
if not defined CHECK_MIN set "CHECK_MIN=15"
if not defined LIVE_SEC set "LIVE_SEC=30"
if not defined MAX_RESTARTS set "MAX_RESTARTS=5"
if not exist "%DIR%" mkdir "%DIR%"

REM ── SP2: csd-relay-node paths ────────────────────────────────────────────────
REM The relay binary is downloaded as a standalone release asset alongside the
REM miner. On Windows it is started with  start /LOW /B  (lowest priority,
REM background/detached) so it NEVER starves the GPU miner.
REM
REM SP2 relay-node launch args (REAL flags confirmed against binary):
REM   --rpc           127.0.0.1:18645        local RPC port (confirmed real flag)
REM   --datadir       %LOCALAPPDATA%\csd-relay  relay chain data dir
REM   --peer-seeds    <comma-sep multiaddrs>  well-known peers (confirmed real flag)
REM   --p2p-listen    /ip4/0.0.0.0/tcp/18644 p2p listen (multiaddr; confirmed real flag)
REM   CSD_RELAY_BLACKLIST_ADDR20 env          addr20 blacklist file (node writes it)
REM   CSD_BLACKLIST_URL env                   signed-blacklist source; ENABLES the node's built-in
REM                                           15-min Ed25519-signed fetcher (pull->verify->write fail-closed)
REM   CSD_CANONICAL_TIP_URL env              canonical oracle
REM   CSD_CANON_REORG_AHEAD env              SP1.1 auth-reorg depth (= 7)
REM
REM NOTE: Windows does not have ionice or taskset equivalents in cmd.exe.
REM /LOW is the practical CPU priority cap available without third-party tools.
REM
REM WALLET: relay requires --wallet (binary hard-rejects absent/zero wallet).
REM The relay never mines (no bridge polls it). If the wallet file is absent,
REM the operator must generate one manually:
REM   csd-relay-node.exe wallet new --out %RELAY_WALLET%
REM TODO(operator): confirm exact subcommand against `csd-relay-node.exe --help`.
REM
set "RELAY_EXE=csd-relay-node.exe"
set "RELAY_BIN=%DIR%\%RELAY_EXE%"
set "RELAY_DATADIR=%LOCALAPPDATA%\csd-relay"
set "RELAY_WALLET=%RELAY_DATADIR%\wallet.json"
set "RELAY_BLACKLIST=%DIR%\relay-blacklist.txt"
set "RELAY_LOG=%DIR%\relay.log"
REM ── end SP2 constants ────────────────────────────────────────────────────────

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

REM --- which GPU device indices to mine ---
REM Default: one process per detected card (0 .. NGPU-1). If CSD_GPU_IDS is set
REM (e.g. "0,2"), mine exactly those indices instead (skip a bad card).
set "GPU_ARG="
if defined CSD_GPU_IDS (
  set "DEVLIST=%CSD_GPU_IDS%"
  set "GPU_ARG=--gpu-id %CSD_GPU_IDS%"
  echo Using CSD_GPU_IDS filter: mining devices %CSD_GPU_IDS%.
) else (
  set "NGPU="
  for /f "usebackq delims=" %%n in (`powershell -NoProfile -Command "$g=@((Get-CimInstance Win32_VideoController).Name); $c=0; foreach($n in $g){ if($n -match 'NVIDIA' -or $n -match 'AMD' -or $n -match 'Radeon'){$c++} }; $c"`) do set "NGPU=%%n"
  if not defined NGPU set "NGPU=1"
  if !NGPU! LSS 1 set "NGPU=1"
  REM Build a space-separated device list 0 1 2 ... NGPU-1.
  set "DEVLIST="
  set /a LAST=!NGPU!-1
  for /l %%i in (0,1,!LAST!) do set "DEVLIST=!DEVLIST! %%i"
  echo Rig has !NGPU! GPU(s).
)
echo Mining to !ADDR!.
echo Auto-checking GitHub for updates every %CHECK_MIN% min (liveness every %LIVE_SEC%s). Keep this open.
echo(

set "INSTALLED=none"
set "RESTARTS=0"
set "BACKOFF=0"
set "HOOK_FIRED=0"
set "ELAPSED=0"

REM Run an update check immediately so we start on the latest published build.
call :update_check

:loop
REM --- fast path: keep the miners alive with escalating backoff ---
if not "!INSTALLED!"=="none" (
  tasklist /FI "IMAGENAME eq %EXE%" 2>nul | find /I "%EXE%" >nul
  if errorlevel 1 (
    REM No miner process is running.
    if !RESTARTS! GEQ %MAX_RESTARTS% (
      if !BACKOFF!==0 ( set "BACKOFF=5" ) else ( set /a BACKOFF=!BACKOFF!*3 )
      if !BACKOFF! GTR 60 set "BACKOFF=60"
      echo [%time%] miners crash-looping ^(!RESTARTS! restarts^) - backing off !BACKOFF!s before retry.
      if !HOOK_FIRED!==0 ( call :run_crash_hook & set "HOOK_FIRED=1" )
      powershell -NoProfile -Command "Start-Sleep -Seconds !BACKOFF!"
    )
    echo [%time%] miners not running - restarting
    call :start_miners
    set /a RESTARTS=!RESTARTS!+1
  ) else (
    REM Healthy this tick: decay the crash-loop state.
    if !RESTARTS! GTR 0 ( set "RESTARTS=0" & set "BACKOFF=0" & set "HOOK_FIRED=0" )
  )
)

REM --- slow path: poll for a new release every CHECK_MIN minutes ---
REM We tick every LIVE_SEC; accumulate elapsed seconds (ELAPSED is initialised
REM to 0 before the loop) and run the update check when we cross CHECK_MIN*60.
set /a ELAPSED=!ELAPSED!+%LIVE_SEC%
set /a UPDATE_EVERY=%CHECK_MIN%*60
if !ELAPSED! GEQ !UPDATE_EVERY! (
  set "ELAPSED=0"
  call :update_check
)

powershell -NoProfile -Command "Start-Sleep -Seconds %LIVE_SEC%"
goto loop

REM ============================================================
REM  Subroutines
REM ============================================================

:update_check
REM FIX #8: resolve the latest version from the releases/latest/download/ CDN
REM asset latest-version.txt, NOT api.github.com. The unauthenticated API caps at
REM 60 req/hr/IP, so ~20 rigs behind ONE public IP (a farm) get HTTP 403, an empty
REM tag, and the whole farm SILENTLY stops updating. The CDN download path has no
REM such per-IP limit. On offline/404 LATEST stays empty and we cleanly no-op
REM (keep mining) - we do NOT fall back to the rate-limited API.
set "LATEST="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "try { $t=(Invoke-WebRequest -Uri 'https://github.com/%REPO%/releases/latest/download/latest-version.txt' -Headers @{'User-Agent'='csd-miner'} -UseBasicParsing).Content; ($t -split \"`n\")[0].Trim().TrimStart('v') } catch { '' }"`) do set "LATEST=%%v"
if not defined LATEST goto :eof

REM Decide whether LATEST is newer than INSTALLED. Prefer the miner's OWN
REM check-update (one tested semver compare: 0.1.10 > 0.1.9). If the installed
REM binary is missing or predates the subcommand (first hardened update), fall
REM back to a plain string inequality.
set "DOUPDATE=0"
if exist "%BIN%" (
  "%BIN%" check-update --help >nul 2>&1
  if !errorlevel!==0 (
    REM Subcommand present: exit 0 means "update available".
    "%BIN%" check-update --current "!INSTALLED!" --latest "!LATEST!" >nul 2>&1
    if !errorlevel!==0 ( set "DOUPDATE=1" )
  ) else (
    if not "!LATEST!"=="!INSTALLED!" set "DOUPDATE=1"
  )
) else (
  if not "!LATEST!"=="!INSTALLED!" set "DOUPDATE=1"
)
if "!DOUPDATE!"=="0" goto :eof

echo [%time%] update: !INSTALLED! -^> !LATEST!  ^(verify, then swap + restart^)

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
set "SUMS=%DIR%\SHA256SUMS.tmp"
if exist "!SUMS!" del /f /q "!SUMS!" >nul 2>&1
curl -L -f -s -o "!SUMS!" "https://github.com/%REPO%/releases/latest/download/SHA256SUMS"
if exist "!SUMS!" (
  REM SHA256SUMS lines are "<hex>  <filename>"; pull the hex for our EXE.
  for /f "usebackq tokens=1,2" %%a in (`findstr /i /e /c:" %EXE%" /c:"*%EXE%" "!SUMS!"`) do set "WANT=%%a"
  del /f /q "!SUMS!" >nul 2>&1
)

REM 3. Verify before swapping. Prefer the TRUSTED running %BIN%'s verify-file -
REM    never let the just-downloaded staged binary verify itself (a malicious
REM    download would pass its own check). FIX C-2: if %BIN% is absent or PREDATES
REM    the verify-file subcommand (a pre-v0.1.8 binary), fall back to PowerShell
REM    Get-FileHash as the OS trusted verifier - mirroring sibling mine-all-gpus.bat
REM    and the Linux launchers' sha256sum fallback - so a pre-verify-file rig can
REM    still verify + auto-advance instead of freezing forever on the old binary.
REM FIX #9: FAIL CLOSED. No SHA256SUMS (or %EXE% not listed), a hash mismatch, or
REM no usable verifier at all REFUSE the update and keep whatever %BIN% exists.
REM Live releases (v0.1.7+) always publish SHA256SUMS, so a missing one is
REM anomalous, not routine. We NEVER swap in an unverified binary.
if not defined WANT (
  echo [%time%] [X] refusing unverified update: no SHA256SUMS published ^(or %EXE% not listed in it^). Keeping the running binary.
  del /f /q "!NEWBIN!" >nul 2>&1
  goto :eof
)
set "VERIFIED=0"
if exist "%BIN%" (
  "%BIN%" verify-file --help >nul 2>&1
  if !errorlevel!==0 (
    "%BIN%" verify-file "!NEWBIN!" "!WANT!" >nul 2>&1
    if !errorlevel!==0 ( set "VERIFIED=1" ) else (
      echo [%time%] [X] SHA-256 verify FAILED for %EXE% - discarding it, keeping the running binary.
      del /f /q "!NEWBIN!" >nul 2>&1
      goto :eof
    )
  )
)
if "!VERIFIED!"=="0" (
  REM No trusted running-binary verifier (first install, or %BIN% predates
  REM verify-file). Use PowerShell Get-FileHash as the OS verifier. FAIL CLOSED if
  REM it is unavailable (empty hash) or the digest does not match.
  set "GOT="
  for /f "usebackq delims=" %%h in (`powershell -NoProfile -Command "try { (Get-FileHash -Algorithm SHA256 -LiteralPath '!NEWBIN!').Hash.ToLower() } catch { '' }"`) do set "GOT=%%h"
  if not defined GOT (
    echo [%time%] [X] refusing unverified update: have a SHA256SUMS digest but no usable verifier ^(no verify-file, Get-FileHash failed^). Keeping current.
    del /f /q "!NEWBIN!" >nul 2>&1
    goto :eof
  )
  if /i not "!GOT!"=="!WANT!" (
    echo [%time%] [X] SHA-256 verify FAILED for %EXE% ^(got !GOT! want !WANT!^) - discarding it.
    del /f /q "!NEWBIN!" >nul 2>&1
    goto :eof
  )
)

REM 4. Verified (or verify intentionally skipped): stop miners + relay, atomically
REM    swap the temp onto the live path, restart (relay re-launched by start_miners).
taskkill /IM "%EXE%" /F >nul 2>&1
REM SP2: also stop the relay so it is re-launched cleanly by :start_miners.
taskkill /IM "%RELAY_EXE%" /F >nul 2>&1
move /Y "!NEWBIN!" "%BIN%" >nul
if not !errorlevel!==0 (
  echo [%time%] [X] could not swap in the new binary; keeping current.
  if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
  goto :eof
)
set "INSTALLED=!LATEST!"
set "RESTARTS=0"
set "BACKOFF=0"
set "HOOK_FIRED=0"
call :start_miners
echo [%time%] now mining !INSTALLED! (build: %VARIANT%).
goto :eof

:start_miners
REM SP2: launch csd-relay-node FIRST at lowest priority (/LOW), detached (/B).
REM Resource cap: /LOW = Windows below-normal priority class (closest to nice 19).
REM The relay has no GPU affinity API on Windows; it runs on whatever cores the
REM scheduler assigns, but /LOW ensures it never preempts the miner threads.
REM
REM Guard: if the relay is already running, tasklist will show it; skip re-launch.
REM (Avoids double-launch on crash-restart of mine-auto.bat.)
if exist "!RELAY_BIN!" (
  tasklist /FI "IMAGENAME eq %RELAY_EXE%" 2>nul | find /I "%RELAY_EXE%" >nul
  if not errorlevel 1 (
    echo [%time%] SP2: relay already running - skipping re-launch.
  ) else (
    echo [%time%] SP2: launching csd-relay-node /LOW /B
    if not exist "!RELAY_DATADIR!" mkdir "!RELAY_DATADIR!"
    REM TODO(operator): if relay wallet absent, generate first:
    REM   csd-relay-node.exe wallet new --out "!RELAY_WALLET!"
    if not exist "!RELAY_WALLET!" (
      echo [%time%] SP2: WARNING - relay wallet not found at !RELAY_WALLET!. Relay may fail to start.
      echo [%time%] SP2: Run: "!RELAY_BIN!" wallet new --out "!RELAY_WALLET!"
    )
    set "CSD_RELAY_BLACKLIST_ADDR20=!RELAY_BLACKLIST!"
    set "CSD_BLACKLIST_URL=https://lisens.yamaduo.no/blacklist"
    set "CSD_CANONICAL_TIP_URL=https://explorer.computesubstrate.org"
    set "CSD_CANON_REORG_AHEAD=7"
    start "CSD relay-node (SP2)" /LOW /B "!RELAY_BIN!" ^
      --rpc 127.0.0.1:18645 ^
      --datadir "!RELAY_DATADIR!" ^
      --wallet "!RELAY_WALLET!" ^
      --peer-seeds /ip4/81.167.197.88/tcp/17999/p2p/12D3KooWA2GFgHLyXSZFVnzuchdesWhqnu7HWw637RXF9P6vW6zK,/ip4/141.94.163.242/tcp/18007/p2p/12D3KooWKGhuUhAwGDf3MtqL581h3gttvFg9Z2p1ej9wFTdKfdSM,/ip4/135.125.170.218/tcp/18007/p2p/12D3KooWSDqQj345ir2Ak5TUKHMn3wPTNsdJCbfPVq66aac29nKt ^
      --p2p-listen /ip4/0.0.0.0/tcp/18644 ^
      >> "!RELAY_LOG!" 2>&1
    echo [%time%] SP2: csd-relay-node started ^(log: !RELAY_LOG!^)
  )
) else (
  echo [%time%] SP2: !RELAY_BIN! not found - relay not started.
  echo [%time%] SP2: Download csd-relay-node.exe into %DIR% to enable relay support.
)
REM Spawn one miner window per device index in DEVLIST.
for %%i in (!DEVLIST!) do start "CSD miner GPU %%i (!INSTALLED!)" "%BIN%" --address !ADDR! --device %%i !GPU_ARG! --log-dir "%DIR%\gpu%%i-log"
goto :eof

:run_crash_hook
if defined CSD_ON_CRASH (
  if exist "!CSD_ON_CRASH!" (
    echo [%time%] running CSD_ON_CRASH hook: !CSD_ON_CRASH!
    call "!CSD_ON_CRASH!"
  ) else (
    echo [%time%] CSD_ON_CRASH set but "!CSD_ON_CRASH!" not found - skipping.
  )
)
goto :eof
