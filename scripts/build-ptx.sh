#!/usr/bin/env bash
set -euo pipefail
# Regenerate src/kernels/sha256d.ptx from sha256d.cu. DEV-ONLY: run after editing
# the kernel. End users never run this - the .ptx is committed and embedded into
# the binary, so the CUDA build and runtime need only the NVIDIA driver (no toolkit).
# Requires the CUDA Toolkit (nvcc) on PATH.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CU="$HERE/../src/kernels/sha256d.cu"
PTX="$HERE/../src/kernels/sha256d.ptx"

if ! command -v nvcc >/dev/null 2>&1; then
  echo "[X] nvcc not found - install the CUDA Toolkit (dev-only)." >&2
  exit 1
fi

echo "Compiling kernel to PTX (arch=compute_75) ..."
nvcc -ptx -arch=compute_75 -maxrregcount=64 --use_fast_math "$CU" -o "$PTX"
echo "[OK] Wrote $PTX"
