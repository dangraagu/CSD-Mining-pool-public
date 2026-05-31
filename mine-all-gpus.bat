@echo off
setlocal EnableExtensions EnableDelayedExpansion
title CSD Pool Miner - all GPUs
color 0a

REM ============================================================
REM  Runs ONE miner instance per GPU for the biggest combined
REM  hashrate. Each instance mines the SAME payout address on a
REM  different --device; the pool sums their shares.
REM  Default = the OpenCL ("amd") build, which drives NVIDIA and
REM  AMD GPUs with just the vendor driver (no CUDA toolkit needed).
REM  Use the CUDA build instead with:  mine-all-gpus.bat nvidia
REM ============================================================

set "REPO=dangraagu/CSD-Mining-pool-public"
set "VARIANT=%~1"
if not defined VARIANT set "VARIANT=amd"
set "DIR=%LOCALAPPDATA%\csd-pool-miner"
set "EXE=csd-pool-miner-%VARIANT%.exe"
set "BIN=%DIR%\%EXE%"
set "CFG=%DIR%\address.txt"
if not exist "%DIR%" mkdir "%DIR%"

echo(
echo  === CSD Pool Miner - all GPUs (build: %VARIANT%) ===
echo(

REM --- 1. download the latest binary ---
echo Downloading %EXE% ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -o "%BIN%" "https://github.com/%REPO%/releases/latest/download/%EXE%"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri 'https://github.com/%REPO%/releases/latest/download/%EXE%' -OutFile '%BIN%' -UseBasicParsing } catch { exit 1 }"
)
if !errorlevel! NEQ 0 ( echo [X] Download failed. & pause & exit /b 1 )

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
echo Detected !NGPU! GPU(s). Launching one miner per GPU to %ADDR% ...
echo(

REM --- 4. spawn one instance per GPU device (0 .. NGPU-1) ---
set /a LAST=!NGPU!-1
for /l %%i in (0,1,!LAST!) do (
  echo   GPU %%i  -^>  new window
  start "CSD miner - GPU %%i" "%BIN%" --address !ADDR! --device %%i --log-dir "%DIR%\gpu%%i-log"
)

echo(
echo Launched !NGPU! window(s), one per GPU, all mining to !ADDR!.
echo Each window shows its own hashrate. Close a window (or Ctrl+C in it)
echo to stop that GPU. A window that closes instantly = that index had no
echo usable GPU (harmless).
pause
endlocal
