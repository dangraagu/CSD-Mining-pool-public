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

REM Release/raw base URLs. Overridable ONLY for hermetic tests (point at a local
REM file:// fake-release dir); unset in normal use, so production always hits
REM GitHub. They control BOTH the binary fetch and the SHA256SUMS fetch.
if not defined BASE_URL set "BASE_URL=https://github.com/%REPO%/releases/latest/download"
if not defined RAW_BASE set "RAW_BASE=https://raw.githubusercontent.com/%REPO%/main"

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

REM === BOOTSTRAP-VERIFY BEGIN ===
REM --- 3. Download the matching miner + SHA-256 VERIFY it (fail-closed) ---
REM We do NOT trust the bootstrap download blindly. Mirroring the steady-state
REM launcher (mine-auto.bat): download to a TEMP (NEVER straight to %BIN%, so an
REM interrupted first download can't leave a truncated %BIN%), look the exe's
REM digest up in the release SHA256SUMS (LF-safe PowerShell extraction - NOT
REM findstr /e, which mis-handles the LF-only file), verify with PowerShell
REM Get-FileHash, and only move it onto %BIN% on a MATCH. A missing SHA256SUMS,
REM the exe not being listed, no Get-FileHash, or a hash MISMATCH all DISCARD the
REM temp and set BOOTSTRAP_OK=0 (never place / run an unverified exe).
REM This block reads DIR / EXE / BIN / BASE_URL and sets BOOTSTRAP_OK (1=placed).
set "BOOTSTRAP_OK=1"
set "DL=%BIN%.download"
if exist "!DL!" del /f /q "!DL!" >nul 2>&1
echo Downloading %EXE% ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -o "!DL!" "%BASE_URL%/%EXE%"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '%BASE_URL%/%EXE%' -OutFile '!DL!' -UseBasicParsing } catch { exit 1 }"
)
if !errorlevel! NEQ 0 (
  echo(
  echo [X] Download failed. Either no release is published yet, the
  echo     '%VARIANT%' build isn't in the latest release, or no network.
  echo     Releases: https://github.com/%REPO%/releases/latest
  echo     Tip: try another build, e.g.  install-csd-miner.bat cpu
  echo(
  if exist "!DL!" del /f /q "!DL!" >nul 2>&1
  set "BOOTSTRAP_OK=0"
  goto :bootstrap_done
)

REM Look up the expected SHA-256 from the release SHA256SUMS (LF-safe; mirrors the
REM HiveOS/Linux awk `$2==a || $2=="*"a {print $1}`): select the line whose EXACT
REM filename field equals %EXE% and emit its 64-hex digest. Any not-found / empty /
REM malformed (non-64-hex) result -> empty WANT -> fail-closed below.
echo Verifying %EXE% against the release SHA256SUMS ...
set "WANT="
set "SUMS=%DIR%\SHA256SUMS.boot"
if exist "!SUMS!" del /f /q "!SUMS!" >nul 2>&1
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -s -o "!SUMS!" "%BASE_URL%/SHA256SUMS"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '%BASE_URL%/SHA256SUMS' -OutFile '!SUMS!' -UseBasicParsing } catch {}"
)
if exist "!SUMS!" (
  for /f "usebackq delims=" %%a in (`powershell -NoProfile -Command "$a='%EXE%'; $h=''; foreach($ln in (Get-Content -LiteralPath '!SUMS!')){ $p=@($ln -split '\s+' ^| Where-Object { $_ -ne '' }); if($p.Count -ge 2 -and ($p[1] -eq $a -or $p[1] -eq ('*'+$a)) -and $p[0] -match '^[0-9A-Fa-f]{64}$'){ $h=$p[0]; break } }; $h"`) do set "WANT=%%a"
  del /f /q "!SUMS!" >nul 2>&1
)
if not defined WANT (
  echo(
  echo [X] Refusing to run an UNVERIFIED miner: no SHA256SUMS published ^(or %EXE%
  echo     is not listed in it^). Every live release publishes SHA256SUMS, so this
  echo     is anomalous. Aborting and NOT running the download.
  echo     Releases: https://github.com/%REPO%/releases/latest
  echo(
  if exist "!DL!" del /f /q "!DL!" >nul 2>&1
  set "BOOTSTRAP_OK=0"
  goto :bootstrap_done
)

REM Verify the temp with the OS verifier (PowerShell Get-FileHash - same as
REM mine-auto.bat). No miner exists yet at bootstrap, so Get-FileHash is the
REM trusted verifier. Empty hash (Get-FileHash unavailable) or a mismatch ->
REM discard + fail-closed.
set "GOT="
for /f "usebackq delims=" %%h in (`powershell -NoProfile -Command "try { (Get-FileHash -Algorithm SHA256 -LiteralPath '!DL!').Hash.ToLower() } catch { '' }"`) do set "GOT=%%h"
if not defined GOT (
  echo(
  echo [X] Have a SHA256SUMS digest but Get-FileHash is unavailable to verify with.
  echo     Refusing to run an unverified miner. Aborting.
  echo(
  if exist "!DL!" del /f /q "!DL!" >nul 2>&1
  set "BOOTSTRAP_OK=0"
  goto :bootstrap_done
)
if /i not "!GOT!"=="!WANT!" (
  echo(
  echo [X] SHA-256 verify FAILED for %EXE% - the download does not match the
  echo     release SHA256SUMS. Discarding it and aborting ^(NOT running it^).
  echo       got:  !GOT!
  echo       want: !WANT!
  echo(
  if exist "!DL!" del /f /q "!DL!" >nul 2>&1
  set "BOOTSTRAP_OK=0"
  goto :bootstrap_done
)
echo   OK - SHA-256 matches ^(!WANT!^).
REM Verified: atomically move the temp onto the live path.
move /Y "!DL!" "%BIN%" >nul 2>&1
if !errorlevel! NEQ 0 (
  echo [X] Could not move the verified miner into place. Aborting.
  if exist "!DL!" del /f /q "!DL!" >nul 2>&1
  set "BOOTSTRAP_OK=0"
)
:bootstrap_done
REM === BOOTSTRAP-VERIFY END ===

if "!BOOTSTRAP_OK!"=="0" (
  pause
  exit /b 1
)

REM --- 3b. Also fetch the multi-GPU + auto-update launchers, AND the OPTIONAL
REM     Windows-service .bat files, next to this file (same RAW_BASE pattern). The
REM     service .bat files are downloaded but NOT run - they are purely opt-in. ---
echo Fetching the multi-GPU / auto-update launchers ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -s -o "%~dp0mine-all-gpus.bat" "%RAW_BASE%/mine-all-gpus.bat"
  curl -L -f -s -o "%~dp0mine-auto.bat" "%RAW_BASE%/mine-auto.bat"
  curl -L -f -s -o "%~dp0install-as-service.bat" "%RAW_BASE%/install-as-service.bat"
  curl -L -f -s -o "%~dp0uninstall-service.bat" "%RAW_BASE%/uninstall-service.bat"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '%RAW_BASE%/mine-all-gpus.bat' -OutFile '%~dp0mine-all-gpus.bat' -UseBasicParsing; Invoke-WebRequest -Uri '%RAW_BASE%/mine-auto.bat' -OutFile '%~dp0mine-auto.bat' -UseBasicParsing; Invoke-WebRequest -Uri '%RAW_BASE%/install-as-service.bat' -OutFile '%~dp0install-as-service.bat' -UseBasicParsing; Invoke-WebRequest -Uri '%RAW_BASE%/uninstall-service.bat' -OutFile '%~dp0uninstall-service.bat' -UseBasicParsing } catch {}"
)
echo   - mine-all-gpus.bat  = mine on ALL GPUs at once
echo   - mine-auto.bat      = all GPUs + auto-update (recommended for 24/7)
echo   - OPTIONAL: double-click install-as-service.bat (will prompt for admin) to run the miner as an auto-start/restart Windows service (optional). Remove it later with uninstall-service.bat.

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

REM Test hook: stop right before the mine-auto.bat hand-off so a hermetic test can
REM assert on the installed %BIN% without launching the mining loop. ZERO effect on
REM a normal run (the var is unset there).
if "%CSD_INSTALL_NO_EXEC%"=="1" (
  echo [test] CSD_INSTALL_NO_EXEC=1 - stopping before mine-auto.bat hand-off.
  endlocal
  exit /b 0
)

REM --- 5. Mine (hand off to the self-updating launcher) ---
REM IMPORTANT: we do NOT run the raw binary here. Stranding a rig on an old
REM version is exactly what this fleet must avoid, so the one-click install ends
REM by handing off to mine-auto.bat - which keeps polling GitHub and swaps in
REM newer VERIFIED builds for as long as its window stays open. mine-auto.bat
REM reuses the address we just saved to %CFG% (no re-prompt) and runs one miner
REM per GPU.
echo(
echo Starting %VARIANT% miner via the self-updating launcher (mine-auto.bat).
echo Payout address: !ADDR!   ^(change it later by deleting: %CFG%^)
echo It auto-checks GitHub for updates and verifies each download before swapping it in.
echo Tip: if no GPU is found, run  "%BIN%" devices  ^(or --list-devices^) to see
echo      which cards this build detects, or reinstall with: install-csd-miner.bat cpu
echo Press Ctrl+C to stop.
echo(
if exist "%~dp0mine-auto.bat" (
  call "%~dp0mine-auto.bat" %VARIANT%
) else (
  REM FAIL-SAFE: launcher missing -> run the installed+verified binary directly
  REM so the rig still mines (just without auto-update). Re-download mine-auto.bat
  REM for 24/7 rigs.
  echo [!] mine-auto.bat not found next to the installer; running the installed
  echo     binary directly ^(no auto-update^).
  "%BIN%" --address !ADDR!
  echo(
  echo Miner stopped.
  pause
)
endlocal
