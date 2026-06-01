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
echo [OK] Wrote "%PTX%"
endlocal
