#!/usr/bin/env bash
set -euo pipefail

# ============================================================
#  CSD Pool Miner - all-in-one installer for Ubuntu / Linux.
#  Run it. It will:
#    1. Detect your GPU (NVIDIA / AMD) or fall back to CPU.
#    2. Download the matching prebuilt miner from GitHub Releases.
#    3. Ask for your addr20 payout address once (and remember it).
#    4. Start mining to the pool.
#  Override detection:  ./install-csd-miner.sh nvidia|amd|cpu
#  GPU DRIVERS ARE NOT INSTALLED HERE - the GPU builds need your
#  vendor driver/runtime already present; otherwise use the cpu build.
#
#  Running via  curl ... | bash  (no terminal)? There is no TTY to
#  prompt on, so pass your address in the environment:
#     curl -fsSL <url> | CSD_ADDR=<addr20> bash
#  or as the second argument:  ... | bash -s -- <variant> <addr20>
# ============================================================

REPO="dangraagu/CSD-Mining-pool-public"

# XDG dirs: binary lives under data, address under config.
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/csd-pool-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/csd-pool-miner"
CFG="$CFG_DIR/address.txt"
mkdir -p "$DATA_DIR" "$CFG_DIR"

echo
echo " === CSD Pool Miner installer (Linux) ==="
echo

# --- helpers ---------------------------------------------------------------

# Download $1 -> $2 atomically using curl (preferred) or wget. We fetch into a
# temp file and only move it into place on success, so a failed/partial
# download can never leave a 0-byte file that later gets chmod+x'd and exec'd.
# Returns non-zero on failure.
download() {
  local url="$1" out="$2" tmp
  tmp="$out.tmp"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp" "$url" && mv "$tmp" "$out"
  else
    echo "[X] Neither 'curl' nor 'wget' is installed. Install one and re-run." >&2
    echo "    Ubuntu/Debian:  sudo apt-get install -y curl" >&2
    return 1
  fi
}

# --- 1. Pick the build variant (arg overrides auto-detect) -----------------
VARIANT="${1:-}"
if [ -z "$VARIANT" ]; then
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
    VARIANT="nvidia"
  elif { command -v lspci >/dev/null 2>&1 && lspci 2>/dev/null | grep -Eiq 'VGA.*(AMD|ATI)|Radeon|Display.*(AMD|ATI)'; } \
       || { command -v clinfo >/dev/null 2>&1 && clinfo 2>/dev/null | grep -Eiq 'AMD|Advanced Micro Devices|Radeon'; }; then
    VARIANT="amd"
  else
    VARIANT="cpu"
  fi
fi

case "$VARIANT" in
  nvidia|amd|cpu) ;;
  *)
    echo "[X] Unknown build '$VARIANT'. Use one of: nvidia | amd | cpu" >&2
    exit 1
    ;;
esac
echo "Selected build: $VARIANT"

# Print the relevant prerequisite hint.
case "$VARIANT" in
  nvidia)
    echo "  -> NVIDIA build: needs a recent NVIDIA driver (CUDA links at runtime;"
    echo "     no CUDA toolkit install needed). Check with: nvidia-smi"
    ;;
  amd)
    echo "  -> AMD/OpenCL build: needs an OpenCL runtime. On Ubuntu/Debian:"
    echo "     sudo apt-get install -y ocl-icd-libopencl1   (plus your vendor's OpenCL package)"
    echo "     Verify with: clinfo"
    ;;
  cpu)
    echo "  -> CPU build: no GPU or driver required."
    ;;
esac

BIN_NAME="csd-pool-miner-linux-$VARIANT"
BIN="$DATA_DIR/$BIN_NAME"
URL="https://github.com/$REPO/releases/latest/download/$BIN_NAME"

# --- 2. Download the matching miner ----------------------------------------
echo
echo "Downloading $BIN_NAME ..."
if ! download "$URL" "$BIN"; then
  echo
  echo "[X] Download failed. Either no release is published yet, the"
  echo "    '$VARIANT' build isn't in the latest release, or no network."
  echo "    Releases: https://github.com/$REPO/releases/latest"
  echo "    Tip: try another build, e.g.  ./install-csd-miner.sh cpu"
  echo
  exit 1
fi
chmod +x "$BIN"

# --- 2b. Also fetch the multi-GPU + auto-update launchers next to this file -
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "Fetching the multi-GPU / auto-update launchers ..."
for f in mine-all-gpus.sh mine-auto.sh; do
  if download "https://raw.githubusercontent.com/$REPO/main/$f" "$SCRIPT_DIR/$f" 2>/dev/null; then
    chmod +x "$SCRIPT_DIR/$f" 2>/dev/null || true
  fi
done
echo "  - mine-all-gpus.sh  = mine on ALL GPUs at once"
echo "  - mine-auto.sh      = all GPUs + auto-update (recommended for 24/7)"

# --- 3. addr20 payout address: prompt once, remember thereafter ------------
ADDR=""
if [ -f "$CFG" ]; then
  ADDR="$(tr -d '[:space:]' < "$CFG")"
fi

if [ -z "$ADDR" ]; then
  # Second positional arg, then $CSD_ADDR, are accepted in any mode and are the
  # ONLY way to supply an address when there is no terminal (e.g. curl | bash,
  # where stdin is the pipe, not a TTY, so `read` would get script bytes/EOF).
  ADDR="${2:-${CSD_ADDR:-}}"
  if [ -z "$ADDR" ]; then
    if [ -t 0 ]; then
      echo
      echo "Enter YOUR addr20 payout address (40 hex characters) - where the"
      echo "pool sends your mining rewards:"
      printf '> '
      read -r ADDR
    else
      echo "[X] No saved address and not a TTY - cannot prompt." >&2
      echo "    Re-run in a terminal, or pass the address non-interactively:" >&2
      echo "      curl -fsSL <url> | CSD_ADDR=<addr20> bash" >&2
      echo "      ... | bash -s -- $VARIANT <addr20>" >&2
      exit 1
    fi
  fi
  ADDR="$(printf '%s' "$ADDR" | tr -d '[:space:]')"
fi

# Validate: optional 0x prefix, then exactly 40 hex chars (lower-cased).
ADDR="$(printf '%s' "$ADDR" | tr '[:upper:]' '[:lower:]')"
ADDR_HEX="${ADDR#0x}"
if ! printf '%s' "$ADDR_HEX" | grep -Eq '^[0-9a-f]{40}$'; then
  echo "[X] '$ADDR' is not a valid addr20." >&2
  echo "    It must be 40 hex characters (an optional 0x prefix is allowed)." >&2
  exit 1
fi
ADDR="$ADDR_HEX"

# Persist the (normalised) address for next time.
printf '%s\n' "$ADDR" > "$CFG"

# --- 4. Mine ---------------------------------------------------------------
echo
echo "Starting $VARIANT miner. Payout address: $ADDR"
echo "(Change it later by deleting: $CFG)"
echo "Press Ctrl+C to stop."
echo
exec "$BIN" --address "$ADDR"
