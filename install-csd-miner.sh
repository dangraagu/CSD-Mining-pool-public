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

# Release/raw base URLs. Overridable ONLY for hermetic tests (point them at a
# local file:// fake-release dir); unset in normal use, so production always hits
# GitHub. They control BOTH the binary fetch and the SHA256SUMS fetch.
BASE_URL="${CSD_BASE_URL:-https://github.com/$REPO/releases/latest/download}"
RAW_BASE="${CSD_RAW_BASE_URL:-https://raw.githubusercontent.com/$REPO/main}"

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

# Fetch the release SHA256SUMS and echo the expected hex digest for $1 (the asset
# basename). Empty output => no SHA256SUMS published OR the asset isn't listed;
# the caller treats empty as "cannot verify" and FAILS CLOSED. Mirrors the same
# helper in the steady-state launcher mine-auto.sh. Line format is
# `<hex>  <filename>` (sha256sum style; the filename may be "*"-prefixed).
expected_sha() {
  local asset="$1" sums
  sums="$DATA_DIR/SHA256SUMS.tmp"
  if download "$BASE_URL/SHA256SUMS" "$sums" 2>/dev/null; then
    awk -v a="$asset" '$2==a || $2=="*"a {print $1; exit}' "$sums"
    rm -f "$sums"
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
URL="$BASE_URL/$BIN_NAME"

# --- 2. Download the matching miner + SHA-256 VERIFY it (fail-closed) -------
# We do NOT trust the bootstrap download blindly. Mirroring the steady-state
# launcher (mine-auto.sh download_verify_swap): fetch to a TEMP, look the variant's
# digest up in the release SHA256SUMS, verify with the OS sha256sum, and only
# chmod+x + move it onto the live path on a MATCH. A missing SHA256SUMS, our asset
# not being listed in it, no sha256sum/shasum available, or a hash MISMATCH all
# DISCARD the temp and abort — we never chmod+x / move / run an unverified binary.
echo
echo "Downloading $BIN_NAME ..."
STAGED="$BIN.download"
rm -f "$STAGED"
if ! download "$URL" "$STAGED"; then
  echo
  echo "[X] Download failed. Either no release is published yet, the"
  echo "    '$VARIANT' build isn't in the latest release, or no network."
  echo "    Releases: https://github.com/$REPO/releases/latest"
  echo "    Tip: try another build, e.g.  ./install-csd-miner.sh cpu"
  echo
  rm -f "$STAGED"
  exit 1
fi

echo "Verifying $BIN_NAME against the release SHA256SUMS ..."
WANT="$(expected_sha "$BIN_NAME")"
if [ -z "$WANT" ]; then
  echo
  echo "[X] Refusing to run an UNVERIFIED miner: no SHA256SUMS published (or" >&2
  echo "    '$BIN_NAME' is not listed in it). Every live release publishes" >&2
  echo "    SHA256SUMS, so this is anomalous. Aborting and NOT running the" >&2
  echo "    download. Releases: https://github.com/$REPO/releases/latest" >&2
  echo
  rm -f "$STAGED"
  exit 1
fi
# Verify with the OS hasher (sha256sum, or shasum -a 256). NO running miner exists
# yet at bootstrap, so the OS hasher is the trusted verifier. If none is available,
# FAIL CLOSED rather than run an unverified binary.
GOT=""
if command -v sha256sum >/dev/null 2>&1; then
  GOT="$(sha256sum "$STAGED" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  GOT="$(shasum -a 256 "$STAGED" | awk '{print $1}')"
else
  echo
  echo "[X] Have a SHA256SUMS digest but no sha256sum/shasum to verify with." >&2
  echo "    Refusing to run an unverified miner. Install coreutils and re-run:" >&2
  echo "      Ubuntu/Debian:  sudo apt-get install -y coreutils" >&2
  echo
  rm -f "$STAGED"
  exit 1
fi
if [ "$GOT" != "$WANT" ]; then
  echo
  echo "[X] SHA-256 verify FAILED for $BIN_NAME — the download does not match the" >&2
  echo "    release SHA256SUMS. Discarding it and aborting (NOT running it)." >&2
  echo "      got:  $GOT" >&2
  echo "      want: $WANT" >&2
  echo
  rm -f "$STAGED"
  exit 1
fi
echo "  OK — SHA-256 matches ($WANT)."
# Verified: make it executable, then atomically move it onto the live path.
chmod +x "$STAGED"
mv "$STAGED" "$BIN"

# --- 2b. Also fetch the multi-GPU + auto-update launchers next to this file -
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
echo "Fetching the multi-GPU / auto-update launchers ..."
for f in mine-all-gpus.sh mine-auto.sh; do
  if download "$RAW_BASE/$f" "$SCRIPT_DIR/$f" 2>/dev/null; then
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

# Test hook: stop right before the mine-auto.sh hand-off so a hermetic test can
# assert on the installed $BIN without launching the mining loop. ZERO effect on a
# normal run (the var is unset there).
if [ "${CSD_INSTALL_NO_EXEC:-0}" = "1" ]; then
  echo "[test] CSD_INSTALL_NO_EXEC=1 — stopping before mine-auto.sh hand-off."
  exit 0
fi

# --- 4. Mine (hand off to the self-updating launcher) ----------------------
# IMPORTANT: we do NOT exec the raw binary here. Stranding a rig on an old
# version is the whole problem this fleet must avoid, so the one-click install
# ends by handing off to mine-auto.sh — which keeps polling GitHub and swaps in
# newer VERIFIED builds for as long as it runs. mine-auto.sh reuses the address
# we just saved to $CFG (no re-prompt) and runs one miner per GPU.
echo
echo "Starting $VARIANT miner via the self-updating launcher (mine-auto.sh)."
echo "Payout address: $ADDR   (change it later by deleting: $CFG)"
echo "It auto-checks GitHub for updates and verifies each download before swapping it in."
echo "Tip: if no GPU is found, run  \"$BIN\" devices  (or --list-devices) to see"
echo "     which cards this build detects, or reinstall with: ./install-csd-miner.sh cpu"
echo "Press Ctrl+C to stop."
echo

MINE_AUTO="$SCRIPT_DIR/mine-auto.sh"
if [ -x "$MINE_AUTO" ] || [ -f "$MINE_AUTO" ]; then
  # FAIL-SAFE: if the self-updating launcher can't start for any reason, fall
  # back to running the binary we just installed+verified so the rig still mines.
  exec bash "$MINE_AUTO" "$VARIANT" || exec "$BIN" --address "$ADDR"
else
  echo "[!] mine-auto.sh not found next to the installer; running the installed"
  echo "    binary directly (no auto-update). Re-download mine-auto.sh for 24/7 rigs."
  exec "$BIN" --address "$ADDR"
fi
