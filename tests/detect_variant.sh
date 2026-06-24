#!/usr/bin/env bash
# tests/detect_variant.sh — FIX 2 + FIX 3 (v0.1.15): shared GPU auto-detection.
#
# THE BUG (franxyz container case): the old auto-detect only treated a host as
# NVIDIA when `nvidia-smi` both existed AND ran. In many GPU containers (and some
# minimal driver installs) nvidia-smi is NOT present even though the NVIDIA device
# nodes (/dev/nvidia0, /dev/nvidiactl) are passed through and libcuda.so IS on the
# loader path. Those rigs were mis-detected as `amd` (via an lspci match) or `cpu`,
# so they downloaded and ran the WRONG build and never used the GPU.
#
# Separately, mine-auto.sh / mine-all-gpus.sh hard-defaulted VARIANT to `amd` when
# called with no build arg — so a no-arg launcher on an NVIDIA box ran the amd
# build. FIX 3 makes the no-arg case call detect_variant() instead.
#
# THE CONTRACT detect_variant() must satisfy:
#   nvidia  if ANY of: nvidia-smi works | /dev/nvidiactl or /dev/nvidia* exists |
#                      `ldconfig -p` lists libcuda.so
#   amd     else if an AMD / OpenCL GPU is detected (lspci AMD/ATI/Radeon, or
#                      clinfo names AMD)
#   cpu     otherwise
#
# Testability: detect_variant() honours CSD_NVIDIA_DEV_GLOB (default /dev/nvidia*)
# so the test can point the "device node" probe at a fake dir without touching the
# real /dev. nvidia-smi / ldconfig / lspci / clinfo are stubbed via a sandbox PATH.
#
# Run:  bash tests/detect_variant.sh
# Exit: 0 = all pass

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

# Extract the detect_variant() function body from a script and source it in an
# isolated subshell with a stubbed environment, then echo what it returns.
#   $1 script   $2 sandbox-bin (PATH override)  $3 nvidia-dev-glob
# Stubs present in $2 decide the outcome; absent stubs => `command -v` misses.
resolve() {
  local script="$1" binpath="$2" devglob="$3"
  awk '/^detect_variant\(\)[[:space:]]*\{/{f=1} f{print} f&&/^\}/{exit}' "$script" \
    > "$SANDBOX/fn.sh"
  if ! grep -q 'detect_variant' "$SANDBOX/fn.sh"; then
    echo "__NO_FUNCTION__"; return
  fi
  PATH="$binpath:/usr/bin:/bin" CSD_NVIDIA_DEV_GLOB="$devglob" \
    bash -c 'set -uo pipefail; source "$1"; detect_variant' _ "$SANDBOX/fn.sh"
}

# Make an executable stub on the sandbox PATH.
stub() { local dir="$1" name="$2" body="$3"; printf '#!/usr/bin/env bash\n%s\n' "$body" > "$dir/$name"; chmod +x "$dir/$name"; }

echo
echo "=== detect_variant() shared GPU detection (FIX 2 + FIX 3) ==="
echo

for SCRIPT in install-csd-miner.sh mine-auto.sh mine-all-gpus.sh; do
  S="$ROOT/$SCRIPT"
  echo "-- $SCRIPT --"

  # ── (1) franxyz container: NO nvidia-smi, but /dev/nvidia0 present => nvidia ──
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"
  DEV="$SANDBOX/dev"; mkdir -p "$DEV"; : > "$DEV/nvidia0"   # fake device node
  # Provide an lspci that WOULD say AMD, to prove the device-node check wins.
  stub "$BIN" lspci 'echo "01:00.0 VGA compatible controller: Advanced Micro Devices Radeon"'
  # NO nvidia-smi, NO ldconfig stub.
  R="$(resolve "$S" "$BIN" "$DEV/nvidia*")"
  [ "$R" = "nvidia" ] \
    && ok "$SCRIPT: no nvidia-smi + /dev/nvidia0 present => nvidia (container case)" \
    || fail "$SCRIPT container case" "expected nvidia, got '$R'"
  rm -rf "$SANDBOX"

  # ── (2) libcuda.so via ldconfig, no smi, no dev node => nvidia ───────────────
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"
  stub "$BIN" ldconfig 'echo "        libcuda.so.1 (libc6,x86-64) => /usr/lib/x86_64-linux-gnu/libcuda.so.1"'
  stub "$BIN" lspci 'echo "01:00.0 VGA compatible controller: Advanced Micro Devices Radeon"'
  R="$(resolve "$S" "$BIN" "$SANDBOX/none/nvidia*")"
  [ "$R" = "nvidia" ] \
    && ok "$SCRIPT: ldconfig libcuda.so present => nvidia" \
    || fail "$SCRIPT ldconfig case" "expected nvidia, got '$R'"
  rm -rf "$SANDBOX"

  # ── (3) only an AMD GPU (lspci), no nvidia signal => amd ─────────────────────
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"
  stub "$BIN" lspci 'echo "03:00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Radeon RX"'
  R="$(resolve "$S" "$BIN" "$SANDBOX/none/nvidia*")"
  [ "$R" = "amd" ] \
    && ok "$SCRIPT: AMD GPU only => amd" \
    || fail "$SCRIPT amd case" "expected amd, got '$R'"
  rm -rf "$SANDBOX"

  # ── (4) nothing detectable => cpu ───────────────────────────────────────────
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"
  stub "$BIN" lspci 'echo "00:02.0 VGA compatible controller: Intel Corporation UHD Graphics"'
  R="$(resolve "$S" "$BIN" "$SANDBOX/none/nvidia*")"
  [ "$R" = "cpu" ] \
    && ok "$SCRIPT: no GPU signal => cpu" \
    || fail "$SCRIPT cpu case" "expected cpu, got '$R'"
  rm -rf "$SANDBOX"
done

# ── FIX 3: no-arg launcher resolves to the DETECTED variant, not hard-coded amd ─
echo "-- FIX 3: no-arg VARIANT resolution --"
for SCRIPT in mine-auto.sh mine-all-gpus.sh; do
  S="$ROOT/$SCRIPT"
  # The launcher must NOT contain a bare `VARIANT="${1:-amd}"` default anymore.
  if grep -Eq 'VARIANT="\$\{1:-amd\}"' "$S"; then
    fail "$SCRIPT no-arg default" "still hard-defaults VARIANT to amd: $(grep -n 'VARIANT="\${1:-amd}"' "$S")"
  else
    ok "$SCRIPT: no hard-coded amd default (calls detect_variant when no arg)"
  fi
  # And it must reference detect_variant for the no-arg path.
  grep -q 'detect_variant' "$S" \
    && ok "$SCRIPT: references detect_variant() for auto-detect" \
    || fail "$SCRIPT detect ref" "no detect_variant call found"
done

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
