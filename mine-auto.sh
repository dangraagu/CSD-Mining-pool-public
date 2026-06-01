#!/usr/bin/env bash
set -euo pipefail

# ============================================================
#  Self-updating, multi-GPU launcher. Leave this running.
#   * Runs one miner instance per GPU (each --device i, all to
#     your address) for the biggest combined hashrate.
#   * Every CHECK_MIN minutes it asks GitHub for the latest
#     release; when a NEW version is published it stops the
#     miners, downloads it, and restarts them automatically.
#   * If the miners die, they get restarted on the next check.
#  Build (default OpenCL/amd = NVIDIA+AMD on just the driver):
#     ./mine-auto.sh nvidia
#  Stop everything: Ctrl+C (this also stops the miners).
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
BIN_NAME="csd-pool-miner-linux-$VARIANT"
BIN="$DATA_DIR/$BIN_NAME"
CHECK_MIN="${CHECK_MIN:-15}"
mkdir -p "$DATA_DIR" "$CFG_DIR"

echo
echo " === CSD Pool Miner - auto-update (build: $VARIANT) ==="
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

# Download the current $VARIANT build into $BIN. If a non-cpu variant's asset is
# missing (download fails / 404 - the amd/OpenCL asset is built
# continue-on-error and may be absent), fall back to the cpu build, which is
# always published, updating VARIANT/BIN_NAME/BIN so the update loop tracks the
# cpu asset thereafter. Returns non-zero only if the cpu build also fails.
download_bin() {
  if download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$BIN"; then
    return 0
  fi
  if [ "$VARIANT" != "cpu" ]; then
    echo "[!] '$VARIANT' build unavailable (download failed / 404). Falling back to the cpu build." >&2
    VARIANT="cpu"
    BIN_NAME="csd-pool-miner-linux-$VARIANT"
    BIN="$DATA_DIR/$BIN_NAME"
    download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$BIN"
    return $?
  fi
  return 1
}

# Query the GitHub API for the latest release tag (empty string on failure).
latest_tag() {
  local api="https://api.github.com/repos/$REPO/releases/latest"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL -H 'User-Agent: csd-miner' "$api" 2>/dev/null \
      | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[^"]*"([^"]+)".*/\1/'
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- --header='User-Agent: csd-miner' "$api" 2>/dev/null \
      | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name"[^"]*"([^"]+)".*/\1/'
  fi
}

# --- payout address (reuse the saved one, else prompt) ---------------------
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

# --- count GPUs ------------------------------------------------------------
# OpenCL: count only GPU devices ('Device Type ... GPU'); a plain 'Device Type'
# match overcounts because clinfo also lists CPU OpenCL devices and repeats the
# field per platform. nvidia: count "GPU N:" lines from nvidia-smi -L.
count_gpus() {
  local n=0
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
    n="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
  elif command -v clinfo >/dev/null 2>&1; then
    n="$(clinfo 2>/dev/null | grep -c 'Device Type.*GPU' || true)"
  fi
  case "$n" in ''|*[!0-9]*) n=1 ;; esac
  [ "$n" -lt 1 ] && n=1
  printf '%s' "$n"
}
NGPU="$(count_gpus)"
LAST=$((NGPU - 1))
echo "Rig has $NGPU GPU(s). Mining to $ADDR."
echo "Auto-checking GitHub for updates every $CHECK_MIN min. Keep this running."
echo

PIDS=()

stop_miners() {
  if [ "${#PIDS[@]}" -gt 0 ]; then
    kill "${PIDS[@]}" 2>/dev/null || true
    wait "${PIDS[@]}" 2>/dev/null || true
  fi
  # Belt and braces: kill any stragglers by binary name.
  pkill -f "$BIN_NAME" 2>/dev/null || true
  PIDS=()
}

start_miners() {
  PIDS=()
  local i LOGDIR
  for i in $(seq 0 "$LAST"); do
    LOGDIR="$DATA_DIR/gpu${i}-log"
    mkdir -p "$LOGDIR"
    "$BIN" --address "$ADDR" --device "$i" --log-dir "$LOGDIR" \
      > "$LOGDIR/stdout.log" 2>&1 &
    PIDS+=("$!")
  done
}

# Are any of our launched miners still alive?
miners_running() {
  local p
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] && kill -0 "$p" 2>/dev/null && return 0
  done
  return 1
}

# Clean shutdown on Ctrl+C / TERM.
cleanup() {
  echo
  echo "Stopping miners ..."
  stop_miners
  exit 0
}
trap cleanup INT TERM

INSTALLED="none"

while true; do
  LATEST="$(latest_tag || true)"

  if [ -n "$LATEST" ] && [ "$LATEST" != "$INSTALLED" ]; then
    echo "[$(date '+%H:%M:%S')] update: $INSTALLED -> $LATEST  (stopping, downloading, restarting)"
    stop_miners
    if download_bin; then
      chmod +x "$BIN"
      INSTALLED="$LATEST"
      start_miners
      echo "[$(date '+%H:%M:%S')] now mining $LATEST on $NGPU GPU(s) (build: $VARIANT)."
    else
      echo "[$(date '+%H:%M:%S')] download failed; keeping current, will retry."
    fi
  fi

  # Restart miners if none are running (crashed / never started).
  if [ "$INSTALLED" != "none" ] && ! miners_running; then
    echo "[$(date '+%H:%M:%S')] miners not running - restarting on $NGPU GPU(s)"
    start_miners
  fi

  sleep "$((CHECK_MIN * 60))"
done
