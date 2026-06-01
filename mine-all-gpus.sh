#!/usr/bin/env bash
set -euo pipefail

# ============================================================
#  Runs ONE miner instance per GPU for the biggest combined
#  hashrate. Each instance mines the SAME payout address on a
#  different --device; the pool sums their shares.
#  Default = the OpenCL ("amd") build, which drives NVIDIA and
#  AMD GPUs with just the vendor driver (no CUDA toolkit needed).
#  Use the CUDA build instead with:  ./mine-all-gpus.sh nvidia
# ============================================================

REPO="dangraagu/CSD-Mining-pool-public"

VARIANT="${1:-amd}"
case "$VARIANT" in
  nvidia|amd|cpu) ;;
  *) echo "[X] Unknown build '$VARIANT'. Use one of: nvidia | amd | cpu" >&2; exit 1 ;;
esac

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/csd-pool-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/csd-pool-miner"
CFG="$CFG_DIR/address.txt"
mkdir -p "$DATA_DIR" "$CFG_DIR"

echo
echo " === CSD Pool Miner - all GPUs (build: $VARIANT) ==="
echo

# Download $1 -> $2 atomically: fetch to a temp file and only move it into
# place on success, so a failed/partial download never leaves a 0-byte binary
# that later gets chmod+x'd and exec'd. Returns non-zero on failure.
download() {
  local url="$1" out="$2" tmp
  tmp="$out.tmp"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp" "$url" && mv "$tmp" "$out"
  else
    echo "[X] Neither 'curl' nor 'wget' is installed." >&2
    return 1
  fi
}

# --- 1. download the latest binary -----------------------------------------
# The amd/OpenCL asset is built continue-on-error and may be missing from a
# release; if the requested variant 404s, fall back to the cpu build, which is
# always published.
BIN_NAME="csd-pool-miner-linux-$VARIANT"
BIN="$DATA_DIR/$BIN_NAME"
echo "Downloading $BIN_NAME ..."
if ! download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$BIN"; then
  if [ "$VARIANT" != "cpu" ]; then
    echo "[!] '$VARIANT' build unavailable (download failed / 404). Falling back to the cpu build." >&2
    VARIANT="cpu"
    BIN_NAME="csd-pool-miner-linux-$VARIANT"
    BIN="$DATA_DIR/$BIN_NAME"
    echo "Downloading $BIN_NAME ..."
    if ! download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$BIN"; then
      echo "[X] Download failed for the cpu build too. (No release yet, or no network.)" >&2
      echo "    Releases: https://github.com/$REPO/releases/latest" >&2
      exit 1
    fi
  else
    echo "[X] Download failed. (No release yet, '$VARIANT' build missing, or no network.)" >&2
    echo "    Releases: https://github.com/$REPO/releases/latest" >&2
    exit 1
  fi
fi
chmod +x "$BIN"

# --- 2. payout address (reuse the saved one, else prompt) ------------------
ADDR=""
if [ -f "$CFG" ]; then
  ADDR="$(tr -d '[:space:]' < "$CFG")"
fi
if [ -z "$ADDR" ]; then
  printf 'Enter your addr20 payout address (40 hex): '
  read -r ADDR
  ADDR="$(printf '%s' "$ADDR" | tr -d '[:space:]')"
fi
ADDR="$(printf '%s' "$ADDR" | tr '[:upper:]' '[:lower:]')"
ADDR="${ADDR#0x}"
if ! printf '%s' "$ADDR" | grep -Eq '^[0-9a-f]{40}$'; then
  echo "[X] '$ADDR' is not a valid addr20 (need 40 hex characters)." >&2
  exit 1
fi
printf '%s\n' "$ADDR" > "$CFG"

# --- 3. count GPUs ---------------------------------------------------------
# nvidia: count "GPU N:" lines from nvidia-smi -L.
# OpenCL: count only GPU devices ('Device Type ... GPU'); a plain 'Device Type'
# match overcounts because clinfo also lists CPU OpenCL devices and repeats the
# field per platform.
NGPU=0
if [ "$VARIANT" = "nvidia" ] && command -v nvidia-smi >/dev/null 2>&1; then
  NGPU="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
elif command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
  NGPU="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
elif command -v clinfo >/dev/null 2>&1; then
  NGPU="$(clinfo 2>/dev/null | grep -c 'Device Type.*GPU' || true)"
fi
# Normalise: must be a positive integer; default to 1.
case "$NGPU" in
  ''|*[!0-9]*) NGPU=1 ;;
esac
[ "$NGPU" -lt 1 ] && NGPU=1
echo "Detected $NGPU GPU(s). Launching one miner per GPU to $ADDR ..."
echo

# --- 4. spawn one instance per GPU device (0 .. NGPU-1) --------------------
PIDS=()
LAST=$((NGPU - 1))
for i in $(seq 0 "$LAST"); do
  LOGDIR="$DATA_DIR/gpu${i}-log"
  mkdir -p "$LOGDIR"
  echo "  GPU $i  ->  background process (log: $LOGDIR)"
  "$BIN" --address "$ADDR" --device "$i" --log-dir "$LOGDIR" \
    > "$LOGDIR/stdout.log" 2>&1 &
  PIDS+=("$!")
done

echo
echo "Launched $NGPU miner process(es), one per GPU, all mining to $ADDR."
echo "PIDs: ${PIDS[*]}"
echo
echo "To stop them all:"
echo "    pkill -f $BIN_NAME"
echo "or stop one:  kill <PID>"
echo "Per-GPU logs are under: $DATA_DIR/gpu<i>-log/"
echo
echo "Waiting on miners (Ctrl+C stops this script; use pkill to stop the miners)."
wait
