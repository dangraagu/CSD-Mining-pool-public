#!/usr/bin/env bash
set -euo pipefail

# ============================================================
#  CSD Pool Miner - one-click WALLET CREATOR for Ubuntu / Linux.
#  Run it. It will:
#    1. Make sure the miner binary is present (download if not).
#    2. Generate a fresh CSD wallet (keypair + addr20) LOCALLY.
#    3. Print your new payout address + save the key to
#       csd-wallet.txt next to this script.
#  The private key is created on THIS machine and is NEVER sent
#  anywhere. BACK IT UP - losing it loses the coins.
#
#  We use the CPU build for this: generating a key needs no GPU
#  and no driver, so it works on any machine.
# ============================================================

REPO="dangraagu/CSD-Mining-pool-public"

# Binary lives under XDG data dir (same place install-csd-miner.sh uses).
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/csd-pool-miner"
mkdir -p "$DATA_DIR"

# Key generation is CPU-only work; the cpu build runs everywhere with no
# GPU/driver requirement. (Same asset naming as install-csd-miner.sh.)
BIN_NAME="csd-pool-miner-linux-cpu"
BIN="$DATA_DIR/$BIN_NAME"
URL="https://github.com/$REPO/releases/latest/download/$BIN_NAME"

# This script's own directory (so csd-wallet.txt lands right next to it).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo
echo " === CSD Pool Miner - create a new wallet (Linux) ==="
echo

# Download $1 -> $2 atomically (curl preferred, wget fallback): fetch to a temp
# file and only move into place on success, so a partial download can never
# leave a 0-byte file that later gets chmod+x'd and exec'd.
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

# --- 1. Ensure the miner binary is present (download if missing) -----------
if [ -x "$BIN" ]; then
  echo "Miner binary already present: $BIN"
else
  echo "Downloading the miner ($BIN_NAME) ..."
  if ! download "$URL" "$BIN"; then
    echo
    echo "[X] Download failed. Either no release is published yet, the"
    echo "    cpu build isn't in the latest release, or there is no network."
    echo "    Releases: https://github.com/$REPO/releases/latest"
    echo
    exit 1
  fi
  chmod +x "$BIN"
fi

# --- 2. Generate the wallet (writes csd-wallet.txt in THIS folder) ---------
echo
echo "Generating your new CSD wallet (this stays on your machine)..."
echo
( cd "$SCRIPT_DIR" && "$BIN" newwallet )

echo
echo " ------------------------------------------------------------"
echo " Your new payout ADDRESS is shown above."
echo " Your private key was saved to:  $SCRIPT_DIR/csd-wallet.txt"
echo
echo " *** BACK UP csd-wallet.txt NOW. ***"
echo " If you lose the private key, the coins are GONE FOREVER."
echo " Only ever share the ADDRESS - never the private key."
echo
echo " Next: start mining with ./install-csd-miner.sh and paste the"
echo " address when asked, or run:  csd-pool-miner --address <addr>"
echo " ------------------------------------------------------------"
echo
