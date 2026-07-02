@echo off
REM Regenerate src\kernels\sha256d.fatbin from sha256d.cu. DEV-ONLY: run after
REM editing the kernel. End users never run this - the .fatbin is committed and
REM embedded into the binary (include_bytes!), so the CUDA build and runtime need
REM only the NVIDIA driver (no toolkit).
REM
REM The fatbin bundles TWO images, both compiled from the SAME .cu:
REM   1. a NATIVE sm_120 cubin  (RTX 50-series / Blackwell SASS, no JIT) and
REM   2. the compute_75 PTX with .version pinned to 6.3 (the PROVEN universal
REM      JIT fallback for EVERY non-sm_120 card, present and future - byte-for-byte
REM      the image scripts\build-ptx.bat has always produced).
REM The driver auto-selects the sm_120 cubin on a 5070 Ti and JITs the compute_75
REM PTX on anything else. cuModuleLoadData ingests the fatbin container directly.
REM
REM Requires: CUDA Toolkit (nvcc, fatbinary, cuobjdump) + Visual Studio C++
REM (cl.exe, located via vswhere).
setlocal
set "HERE=%~dp0"
set "CU=%HERE%..\src\kernels\sha256d.cu"
set "PTX=%HERE%..\src\kernels\sha256d.ptx"
set "CUBIN=%HERE%..\src\kernels\sha256d.sm120.cubin"
set "FATBIN=%HERE%..\src\kernels\sha256d.fatbin"

set "CUDA_BIN=C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v13.3\bin"
set "NVCC=%CUDA_BIN%\nvcc.exe"
set "FATBINARY=%CUDA_BIN%\fatbinary.exe"
set "CUOBJDUMP=%CUDA_BIN%\cuobjdump.exe"

if not exist "%NVCC%" ( echo [X] nvcc not found at "%NVCC%" & exit /b 1 )
if not exist "%FATBINARY%" ( echo [X] fatbinary not found at "%FATBINARY%" & exit /b 1 )
if not exist "%CUOBJDUMP%" ( echo [X] cuobjdump not found at "%CUOBJDUMP%" & exit /b 1 )

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
  echo [X] vswhere not found - install Visual Studio with the C++ workload.
  exit /b 1
)
for /f "usebackq delims=" %%i in (`"%VSWHERE%" -latest -property installationPath`) do set "VSPATH=%%i"
call "%VSPATH%\VC\Auxiliary\Build\vcvars64.bat" || exit /b 1

REM (A) Regenerate the compute_75 PTX with .version PINNED to 6.3 by reusing the
REM proven build-ptx.bat verbatim (keeps ONE source of truth for the pin step).
echo === Regenerating compute_75 PTX (.version 6.3 pin) via build-ptx.bat ===
call "%HERE%build-ptx.bat" || ( echo [X] build-ptx.bat failed & exit /b 1 )

REM (B) Native sm_120 cubin. --use_fast_math MATCHES the PTX build (build-ptx.bat
REM uses it too) so both images compute the identical SHA-256d. -maxrregcount=128
REM is the A/B-proven register cap for the Blackwell SASS. sha256d is pure integer
REM ops, so --use_fast_math is inert to the hash (selftest is the bit-exact gate).
echo === Compiling native sm_120 cubin (-maxrregcount=128 --use_fast_math) ===
"%NVCC%" -cubin -arch=sm_120 -maxrregcount=128 --use_fast_math "%CU%" -o "%CUBIN%"
if errorlevel 1 ( echo [X] nvcc -cubin failed & exit /b 1 )

REM (C) Assemble the fatbin. fatbinary 13.3 syntax is --image3=kind=..,sm=NN,file=..
REM (the numeric arch, NOT profile=sm_120). The cubin (native SASS) is kind=elf;
REM the PTX is kind=ptx. --compress-mode=none keeps the images raw + inspectable
REM and the driver JIT-selection path unambiguous. -64 = 64-bit.
echo === Assembling fatbin (sm_120 cubin + compute_75 ptx) ===
"%FATBINARY%" -64 --compress-mode=none --create="%FATBIN%" ^
  --image3=kind=elf,sm=120,file="%CUBIN%" ^
  --image3=kind=ptx,sm=75,file="%PTX%"
if errorlevel 1 ( echo [X] fatbinary failed & exit /b 1 )

REM (D) PROVE the contents. MUST show: one sm_120 SASS image AND a compute_75 PTX
REM whose header reads .version 6.3 / .target sm_75. If .version is 9.x the pin
REM regressed (older 2080-fleet drivers would reject it) -> STOP and fix build-ptx.
echo.
echo === cuobjdump: images present in the fatbin ===
"%CUOBJDUMP%" "%FATBIN%"
echo.
echo === cuobjdump: sm_120 SASS present (native Blackwell path) ===
"%CUOBJDUMP%" -sass "%FATBIN%" | findstr /C:"sm_120" /C:"arch = sm_120"
echo.
echo === cuobjdump: embedded compute_75 PTX header (.version MUST be 6.3) ===
"%CUOBJDUMP%" -ptx "%FATBIN%" | findstr /C:".version" /C:".target"
echo.
echo [OK] Wrote "%FATBIN%"
echo     Verify above: exactly one sm_120 cubin + one compute_75 ptx (.version 6.3).
endlocal
