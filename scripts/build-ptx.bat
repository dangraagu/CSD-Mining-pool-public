@echo off
REM Regenerate src\kernels\sha256d.ptx from sha256d.cu. DEV-ONLY: run after editing
REM the kernel. End users never run this - the .ptx is committed and embedded into
REM the binary, so the CUDA build and runtime need only the NVIDIA driver (no toolkit).
REM Requires: CUDA Toolkit (nvcc) + Visual Studio C++ (cl.exe, located via vswhere).
setlocal
set "HERE=%~dp0"
set "CU=%HERE%..\src\kernels\sha256d.cu"
set "PTX=%HERE%..\src\kernels\sha256d.ptx"

set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if not exist "%VSWHERE%" (
  echo [X] vswhere not found - install Visual Studio with the C++ workload.
  exit /b 1
)
for /f "usebackq delims=" %%i in (`"%VSWHERE%" -latest -property installationPath`) do set "VSPATH=%%i"
call "%VSPATH%\VC\Auxiliary\Build\vcvars64.bat" || exit /b 1

echo Compiling kernel to PTX (arch=compute_75) ...
nvcc -ptx -arch=compute_75 -maxrregcount=64 --use_fast_math "%CU%" -o "%PTX%"
if errorlevel 1 ( echo [X] nvcc failed & exit /b 1 )

REM nvcc stamps the toolkit's PTX ISA into .version, which is too NEW for older
REM NVIDIA drivers (they reject it with CUDA_ERROR_UNSUPPORTED_PTX_VERSION and the
REM miner silently falls back to CPU). The SHA-256d kernel uses only old integer
REM instructions, so pin .version down to 6.3 (the sm_75 floor) for broad driver
REM compatibility. If you later edit the kernel and selftest shows the cuda backend
REM failing to load, the kernel gained a newer instruction -- raise this number.
powershell -NoProfile -Command "$p='%PTX%'; $t=[IO.File]::ReadAllText($p); $t=[Text.RegularExpressions.Regex]::Replace($t,'\.version \d+\.\d+','.version 6.3'); [IO.File]::WriteAllText($p,$t,(New-Object Text.UTF8Encoding $false))"
echo Pinned PTX .version to 6.3 for broad driver compatibility.
echo [OK] Wrote "%PTX%"
endlocal
