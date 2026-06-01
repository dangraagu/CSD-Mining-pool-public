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

# nvcc stamps the toolkit's PTX ISA into .version, which is too NEW for older
# NVIDIA drivers (they reject it with CUDA_ERROR_UNSUPPORTED_PTX_VERSION and the
# miner silently falls back to CPU). The SHA-256d kernel uses only old integer
# instructions, so pin .version down to 6.3 (the sm_75 floor) for broad driver
# compatibility. If you later edit the kernel and selftest shows the cuda backend
# failing to load, the kernel gained a newer instruction -- raise this number.
sed -i 's/^\.version .*/.version 6.3/' "$PTX"
echo "Pinned PTX .version to 6.3 for broad driver compatibility."
echo "[OK] Wrote $PTX"
